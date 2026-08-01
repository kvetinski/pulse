use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};

use crate::application::service::scenario_plan_fingerprint;
use crate::domain::contracts::{
    CURRENT_CONTRACT_VERSION, FailedScenarioJob, JobLoadConfig, JobSlice, MAX_CONTRACT_ATTEMPT,
    MAX_CONTRACT_ID_BYTES, MAX_CONTRACT_SLICES, MAX_POISON_EVIDENCE_FIELD_BYTES,
    PoisonMessageRecord, ScenarioJob, build_terminal_event_id, validate_contract_version,
};
use crate::domain::scenario::Scenario;

/// The shared DLQ carries terminal failed jobs and poison-message evidence.
/// Only failed jobs are replayable; recognized poison records are deliberately
/// skipped by the replay consumer because the original malformed payload cannot
/// be reconstructed safely.
#[derive(Clone, Debug)]
pub enum ReplayRecord {
    FailedJob(FailedScenarioJob),
    Poison(PoisonMessageRecord),
}

/// Classifies a DLQ record without treating arbitrary JSON as skippable poison.
///
/// Failed-job and poison envelopes predate an explicit tagged union, so the
/// unique identity fields are used as a conservative discriminator. Ambiguous,
/// malformed, unsupported, and unknown envelopes fail closed; their replay
/// consumer-group offset must remain unsettled for operator inspection.
pub fn decode_replay_record(payload: &[u8]) -> Result<ReplayRecord, String> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|error| format!("invalid DLQ JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "DLQ payload must be a JSON object".to_string())?;

    let failed_markers = ["scenario_id", "run_id", "execution_key", "slice"];
    let poison_markers = ["source_topic", "source_partition", "source_offset"];
    let looks_failed = failed_markers
        .iter()
        .any(|field| object.contains_key(*field));
    let looks_poison = poison_markers
        .iter()
        .any(|field| object.contains_key(*field));

    match (looks_failed, looks_poison) {
        (true, false) => {
            reject_unknown_fields(
                object,
                &[
                    "schema_version",
                    "event_id",
                    "scenario_id",
                    "run_id",
                    "execution_key",
                    "slice",
                    "failed_at_unix_ms",
                    "attempt",
                    "max_retries",
                    "reason",
                ],
                "failed-job",
            )?;
            let failed: FailedScenarioJob = serde_json::from_value(value)
                .map_err(|error| format!("invalid failed-job DLQ envelope: {error}"))?;
            validate_failed_job(&failed)?;
            Ok(ReplayRecord::FailedJob(failed))
        }
        (false, true) => {
            reject_unknown_fields(
                object,
                &[
                    "schema_version",
                    "event_id",
                    "failed_at_unix_ms",
                    "source_topic",
                    "source_partition",
                    "source_offset",
                    "source_key_base64",
                    "source_key_original_bytes",
                    "source_key_truncated",
                    "payload_base64",
                    "payload_original_bytes",
                    "payload_truncated",
                    "reason",
                ],
                "poison-message",
            )?;
            let poison: PoisonMessageRecord = serde_json::from_value(value)
                .map_err(|error| format!("invalid poison-message DLQ envelope: {error}"))?;
            validate_poison_record(&poison)?;
            Ok(ReplayRecord::Poison(poison))
        }
        (true, true) => Err(
            "ambiguous DLQ payload contains both failed-job and poison-message identity fields"
                .to_string(),
        ),
        (false, false) => Err("unknown DLQ envelope shape".to_string()),
    }
}

fn reject_unknown_fields(
    object: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
    kind: &str,
) -> Result<(), String> {
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("unknown {kind} DLQ field: {field}"));
    }
    Ok(())
}

fn validate_failed_job(job: &FailedScenarioJob) -> Result<(), String> {
    validate_contract_version(job.schema_version)
        .map_err(|error| format!("invalid failed-job contract: {error}"))?;
    validate_text("scenario_id", &job.scenario_id)?;
    validate_text("run_id", &job.run_id)?;
    validate_text("execution_key", &job.execution_key)?;
    if job.schema_version >= 2 {
        validate_text("event_id", &job.event_id)?;
        let expected = crate::domain::contracts::build_terminal_event_id(
            &job.execution_key,
            job.attempt,
            "dlq",
        );
        if job.event_id != expected {
            return Err(
                "failed-job event_id is not the deterministic terminal identity".to_string(),
            );
        }
    }
    validate_text("reason", &job.reason)?;
    validate_slice(&job.slice)?;
    if job.attempt > job.max_retries || job.max_retries > MAX_CONTRACT_ATTEMPT {
        return Err(format!(
            "invalid failed-job attempt metadata: attempt={}, max_retries={}",
            job.attempt, job.max_retries
        ));
    }
    Ok(())
}

fn validate_poison_record(record: &PoisonMessageRecord) -> Result<(), String> {
    validate_contract_version(record.schema_version)
        .map_err(|error| format!("invalid poison-message contract: {error}"))?;
    validate_text("event_id", &record.event_id)?;
    validate_text("source_topic", &record.source_topic)?;
    validate_text("reason", &record.reason)?;
    if record.source_partition < 0 {
        return Err("poison source_partition must be non-negative".to_string());
    }
    if record.source_offset < 0 {
        return Err("poison source_offset must be non-negative".to_string());
    }
    let expected_event_id = format!(
        "poison:{}:{}:{}",
        record.source_topic, record.source_partition, record.source_offset
    );
    if record.event_id != expected_event_id {
        return Err("poison event_id is not the deterministic source identity".to_string());
    }
    validate_poison_evidence(
        "source_key",
        record.source_key_base64.as_deref(),
        record.source_key_original_bytes,
        record.source_key_truncated,
    )?;
    validate_poison_evidence(
        "payload",
        record.payload_base64.as_deref(),
        record.payload_original_bytes,
        record.payload_truncated,
    )?;
    Ok(())
}

fn validate_poison_evidence(
    field: &str,
    encoded: Option<&str>,
    original_bytes: Option<u64>,
    truncated: bool,
) -> Result<(), String> {
    let Some(encoded) = encoded else {
        if original_bytes.is_some() || truncated {
            return Err(format!(
                "poison {field} evidence metadata exists without encoded evidence"
            ));
        }
        return Ok(());
    };
    let max_encoded_bytes = MAX_POISON_EVIDENCE_FIELD_BYTES.div_ceil(3) * 4;
    if encoded.len() > max_encoded_bytes {
        return Err(format!(
            "poison {field} evidence exceeds its bounded prefix"
        ));
    }
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid poison {field}_base64: {error}"))?;
    if decoded.len() > MAX_POISON_EVIDENCE_FIELD_BYTES {
        return Err(format!(
            "poison {field} evidence exceeds its bounded prefix"
        ));
    }

    let Some(original_bytes) = original_bytes else {
        // Legacy poison envelopes did not record original lengths. They remain
        // inspectable only when they do not claim truncation.
        return if truncated {
            Err(format!(
                "poison {field} claims truncation without an original byte count"
            ))
        } else {
            Ok(())
        };
    };
    let retained_bytes = u64::try_from(decoded.len()).unwrap_or(u64::MAX);
    if (truncated && original_bytes <= retained_bytes)
        || (!truncated && original_bytes != retained_bytes)
    {
        return Err(format!(
            "poison {field} original byte count is inconsistent with retained evidence"
        ));
    }
    Ok(())
}

fn validate_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("DLQ {field} must not be empty"));
    }
    if value.len() > MAX_CONTRACT_ID_BYTES {
        return Err(format!("DLQ {field} exceeds {MAX_CONTRACT_ID_BYTES} bytes"));
    }
    Ok(())
}

fn validate_slice(slice: &JobSlice) -> Result<(), String> {
    if slice.total == 0 || slice.total > MAX_CONTRACT_SLICES || slice.index >= slice.total {
        return Err(format!(
            "invalid failed-job slice {} of {}",
            slice.index, slice.total
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct ReplayFilters {
    pub scenario_ids: HashSet<String>,
    pub reason_contains: Option<String>,
    pub since_unix_ms: Option<u128>,
    pub until_unix_ms: Option<u128>,
    pub limit: Option<usize>,
}

impl ReplayFilters {
    pub fn matches(&self, job: &FailedScenarioJob) -> bool {
        if !self.scenario_ids.is_empty() && !self.scenario_ids.contains(&job.scenario_id) {
            return false;
        }

        if let Some(since) = self.since_unix_ms
            && job.failed_at_unix_ms < since
        {
            return false;
        }

        if let Some(until) = self.until_unix_ms
            && job.failed_at_unix_ms > until
        {
            return false;
        }

        if let Some(needle) = &self.reason_contains {
            let haystack = job.reason.to_lowercase();
            let needle = needle.to_lowercase();
            if !haystack.contains(&needle) {
                return false;
            }
        }

        true
    }
}

#[derive(Clone, Debug)]
pub struct ReplayOptions {
    pub rate_per_sec: f64,
    pub scale: f64,
    pub worker_max_retries: u32,
    pub execution_semantics_fingerprint: String,
    pub dry_run: bool,
    pub idempotent_ack: bool,
}

/// Immutable Kafka coordinates of the DLQ record being replayed.
///
/// Replay identities are derived from these coordinates rather than wall-clock
/// time. If publishing the replacement job succeeds but committing this source
/// offset does not, the redelivery therefore produces the same replacement
/// identity instead of generating target traffic under a second identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaySource {
    topic: String,
    partition: i32,
    offset: i64,
}

impl ReplaySource {
    pub fn new(topic: impl Into<String>, partition: i32, offset: i64) -> Result<Self, String> {
        let topic = topic.into();
        if topic.trim().is_empty() {
            return Err("replay source topic must not be empty".to_string());
        }
        // Kafka topic names are capped at 249 bytes. Enforcing that boundary
        // here also proves that the base64-encoded topic fits the contract ID.
        if topic.len() > 249 {
            return Err("replay source topic exceeds Kafka's 249-byte limit".to_string());
        }
        if partition < 0 {
            return Err("replay source partition must be non-negative".to_string());
        }
        if offset < 0 {
            return Err("replay source offset must be non-negative".to_string());
        }
        Ok(Self {
            topic,
            partition,
            offset,
        })
    }
}

pub fn build_replay_job(
    failed: &FailedScenarioJob,
    source: &ReplaySource,
    scenario: &Scenario,
    global_startup_burst: usize,
    options: &ReplayOptions,
) -> Result<ScenarioJob, String> {
    if options.scale != 1.0 {
        return Err(
            "v2 replay scale must be exactly 1.0 so the job matches the deterministic local plan"
                .to_string(),
        );
    }
    if global_startup_burst > scenario.config.max_concurrency {
        return Err(format!(
            "startup burst {global_startup_burst} exceeds scenario concurrency {}",
            scenario.config.max_concurrency
        ));
    }
    // A replayed DLQ record is a new one-slice run. Keeping the old slice total
    // while assigning a new run ID would create an aggregate that can never
    // complete because the sibling slices belong to different replay runs.
    let replay_slice = JobSlice { index: 0, total: 1 };
    let failed_event_identity = if failed.event_id.is_empty() {
        // V1 failed-job records predate event_id. Their original execution and
        // attempt still provide a stable terminal-event identity.
        build_terminal_event_id(&failed.execution_key, failed.attempt, "dlq")
    } else {
        failed.event_id.clone()
    };
    let encoded_topic = URL_SAFE_NO_PAD.encode(source.topic.as_bytes());
    let run_id = format!(
        "v{CURRENT_CONTRACT_VERSION}:replay:t{encoded_topic}:p{}:o{}:e{:032x}",
        source.partition,
        source.offset,
        fnv1a_128(failed_event_identity.as_bytes())
    );
    let execution_key = format!("{run_id}:slice-0-of-1");
    for (field, value) in [
        ("replay run_id", run_id.as_str()),
        ("replay execution_key", execution_key.as_str()),
    ] {
        if value.len() > MAX_CONTRACT_ID_BYTES {
            return Err(format!(
                "{field} exceeds the {MAX_CONTRACT_ID_BYTES}-byte contract identity bound"
            ));
        }
    }

    Ok(ScenarioJob {
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: failed.scenario_id.clone(),
        run_id,
        execution_key,
        plan_fingerprint: scenario_plan_fingerprint(
            scenario,
            1,
            global_startup_burst,
            options.worker_max_retries,
            &options.execution_semantics_fingerprint,
        ),
        // FailedScenarioJob does not carry the original scheduled timestamp.
        // Its durable failure time is the only stable wall-clock metadata in
        // both supported contract versions, so replay preserves that value.
        scheduled_at_unix_ms: failed.failed_at_unix_ms,
        not_before_unix_ms: 0,
        slice: replay_slice,
        load: JobLoadConfig {
            scenarios_per_sec: scenario.config.scenarios_per_sec,
            duration: scenario.config.duration,
            max_concurrency: scenario.config.max_concurrency,
            startup_burst: global_startup_burst,
        },
        attempt: 0,
        max_retries: options.worker_max_retries,
    })
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(*byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::{
        ReplayFilters, ReplayOptions, ReplayRecord, ReplaySource, build_replay_job,
        decode_replay_record,
    };
    use crate::domain::contracts::{
        CURRENT_CONTRACT_VERSION, FailedScenarioJob, JobSlice, PoisonMessageRecord,
    };
    use crate::domain::contracts::{PartitionKeyStrategy, ScenarioJob};
    use crate::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig};

    fn sample_failed() -> FailedScenarioJob {
        FailedScenarioJob {
            schema_version: 1,
            event_id: String::new(),
            scenario_id: "AccountFlow".to_string(),
            run_id: "run-1".to_string(),
            execution_key: "AccountFlow:123:slice-0-of-2".to_string(),
            slice: JobSlice { index: 0, total: 2 },
            failed_at_unix_ms: 1_000,
            attempt: 1,
            max_retries: 2,
            reason: "timeout".to_string(),
        }
    }

    fn sample_scenario() -> Scenario {
        Scenario::new(
            "AccountFlow",
            Vec::new(),
            ScenarioConfig {
                endpoint: "http://127.0.0.1:8080".to_string(),
                scenarios_per_sec: 20.0,
                max_concurrency: 10,
                duration: Duration::from_secs(30),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        )
    }

    fn assert_new_execution_key(job: &ScenarioJob) {
        assert!(job.execution_key.starts_with(&format!(
            "v{CURRENT_CONTRACT_VERSION}:replay:tdGVzdC5kbHE:p2:o41:e"
        )));
        assert!(job.execution_key.contains("slice-0-of-1"));
    }

    fn sample_source(offset: i64) -> ReplaySource {
        ReplaySource::new("test.dlq", 2, offset).expect("valid source coordinates")
    }

    #[test]
    fn filters_match_by_scenario_reason_and_time() {
        let job = sample_failed();
        let mut ids = HashSet::new();
        ids.insert("AccountFlow".to_string());

        let filters = ReplayFilters {
            scenario_ids: ids,
            reason_contains: Some("time".to_string()),
            since_unix_ms: Some(500),
            until_unix_ms: Some(2_000),
            limit: None,
        };

        assert!(filters.matches(&job));
    }

    #[test]
    fn filters_reject_mismatched_scenario() {
        let job = sample_failed();
        let mut ids = HashSet::new();
        ids.insert("Other".to_string());

        let filters = ReplayFilters {
            scenario_ids: ids,
            reason_contains: None,
            since_unix_ms: None,
            until_unix_ms: None,
            limit: None,
        };

        assert!(!filters.matches(&job));
    }

    #[test]
    fn build_replay_job_resets_attempt_and_stamps_current_plan() {
        let failed = sample_failed();
        let scenario = sample_scenario();
        let options = ReplayOptions {
            rate_per_sec: 5.0,
            scale: 1.0,
            worker_max_retries: 3,
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
            dry_run: true,
            idempotent_ack: false,
        };

        let job = build_replay_job(&failed, &sample_source(41), &scenario, 2, &options)
            .expect("valid replay plan");

        assert_eq!(job.attempt, 0);
        assert_eq!(job.max_retries, 3);
        assert_eq!(job.schema_version, CURRENT_CONTRACT_VERSION);
        assert_eq!(job.slice, JobSlice { index: 0, total: 1 });
        assert_eq!(job.scheduled_at_unix_ms, failed.failed_at_unix_ms);
        assert_new_execution_key(&job);
        assert_eq!(job.load.scenarios_per_sec, 20.0);
        assert_eq!(job.load.max_concurrency, 10);
        assert_eq!(job.load.startup_burst, 2);
        assert!(!job.plan_fingerprint.is_empty());
    }

    #[test]
    fn replay_rejects_load_scaling_that_breaks_the_v2_plan() {
        let options = ReplayOptions {
            rate_per_sec: 5.0,
            scale: 0.25,
            worker_max_retries: 3,
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
            dry_run: true,
            idempotent_ack: false,
        };
        assert!(
            build_replay_job(
                &sample_failed(),
                &sample_source(41),
                &sample_scenario(),
                0,
                &options,
            )
            .expect_err("scaled job must fail")
            .contains("exactly 1.0")
        );
    }

    #[test]
    fn replay_identity_is_stable_for_source_redelivery() {
        let failed = sample_failed();
        let scenario = sample_scenario();
        let source = sample_source(41);
        let options = ReplayOptions {
            rate_per_sec: 5.0,
            scale: 1.0,
            worker_max_retries: 3,
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
            dry_run: false,
            idempotent_ack: true,
        };

        let first =
            build_replay_job(&failed, &source, &scenario, 2, &options).expect("first replay job");
        let redelivery = build_replay_job(&failed, &source, &scenario, 2, &options)
            .expect("redelivered replay job");

        assert_eq!(first.run_id, redelivery.run_id);
        assert_eq!(first.execution_key, redelivery.execution_key);
        assert_eq!(first.scheduled_at_unix_ms, redelivery.scheduled_at_unix_ms);
    }

    #[test]
    fn different_source_offsets_cannot_share_a_replay_identity() {
        let failed = sample_failed();
        let scenario = sample_scenario();
        let options = ReplayOptions {
            rate_per_sec: 5.0,
            scale: 1.0,
            worker_max_retries: 3,
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
            dry_run: false,
            idempotent_ack: true,
        };

        let first = build_replay_job(&failed, &sample_source(41), &scenario, 2, &options)
            .expect("first replay job");
        let second = build_replay_job(&failed, &sample_source(42), &scenario, 2, &options)
            .expect("second replay job");

        assert_ne!(first.run_id, second.run_id);
        assert_ne!(first.execution_key, second.execution_key);
        assert!(first.run_id.contains(":o41:"));
        assert!(second.run_id.contains(":o42:"));
    }

    #[test]
    fn failed_event_identity_is_bound_into_replay_identity() {
        let mut first_failed = sample_failed();
        first_failed.schema_version = CURRENT_CONTRACT_VERSION;
        first_failed.event_id = crate::domain::contracts::build_terminal_event_id(
            &first_failed.execution_key,
            first_failed.attempt,
            "dlq",
        );
        let mut second_failed = first_failed.clone();
        second_failed.execution_key = "AccountFlow:456:slice-0-of-2".to_string();
        second_failed.event_id = crate::domain::contracts::build_terminal_event_id(
            &second_failed.execution_key,
            second_failed.attempt,
            "dlq",
        );
        let options = ReplayOptions {
            rate_per_sec: 5.0,
            scale: 1.0,
            worker_max_retries: 3,
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
            dry_run: false,
            idempotent_ack: true,
        };

        let first = build_replay_job(
            &first_failed,
            &sample_source(41),
            &sample_scenario(),
            0,
            &options,
        )
        .expect("first replay job");
        let second = build_replay_job(
            &second_failed,
            &sample_source(41),
            &sample_scenario(),
            0,
            &options,
        )
        .expect("second replay job");

        assert_ne!(first.run_id, second.run_id);
        assert_ne!(first.execution_key, second.execution_key);
    }

    #[test]
    fn replay_identity_is_bounded_at_the_kafka_topic_limit() {
        let source = ReplaySource::new("t".repeat(249), i32::MAX, i64::MAX)
            .expect("maximum Kafka coordinates");
        let job = build_replay_job(
            &sample_failed(),
            &source,
            &sample_scenario(),
            0,
            &ReplayOptions {
                rate_per_sec: 5.0,
                scale: 1.0,
                worker_max_retries: 3,
                execution_semantics_fingerprint: "test-runtime-v1".to_string(),
                dry_run: false,
                idempotent_ack: true,
            },
        )
        .expect("bounded replay job");

        assert!(job.run_id.len() <= crate::domain::contracts::MAX_CONTRACT_ID_BYTES);
        assert!(job.execution_key.len() <= crate::domain::contracts::MAX_CONTRACT_ID_BYTES);
        job.validate().expect("replay job contract");
    }

    #[test]
    fn mixed_failed_and_poison_envelopes_can_be_scanned_in_order() {
        let first_failed = serde_json::to_vec(&sample_failed()).expect("failed job serializes");
        let poison = PoisonMessageRecord {
            schema_version: CURRENT_CONTRACT_VERSION,
            event_id: "poison:pulse.scenario.jobs:2:41".to_string(),
            failed_at_unix_ms: 1_000,
            source_topic: "pulse.scenario.jobs".to_string(),
            source_partition: 2,
            source_offset: 41,
            source_key_base64: Some("am9iLWtleQ==".to_string()),
            source_key_original_bytes: Some(7),
            source_key_truncated: false,
            payload_base64: Some("e25vdC1qc29ufQ==".to_string()),
            payload_original_bytes: Some(10),
            payload_truncated: false,
            reason: "invalid scenario job JSON".to_string(),
        };
        let poison = serde_json::to_vec(&poison).expect("poison record serializes");
        let second_failed = serde_json::to_vec(&sample_failed()).expect("failed job serializes");

        let classified = [&first_failed, &poison, &second_failed]
            .map(|payload| decode_replay_record(payload).expect("known envelope"));
        assert!(matches!(&classified[0], ReplayRecord::FailedJob(_)));
        assert!(matches!(&classified[1], ReplayRecord::Poison(_)));
        assert!(matches!(&classified[2], ReplayRecord::FailedJob(_)));
    }

    #[test]
    fn version_two_failed_job_requires_deterministic_event_identity() {
        let mut failed = sample_failed();
        failed.schema_version = CURRENT_CONTRACT_VERSION;
        failed.event_id = crate::domain::contracts::build_terminal_event_id(
            &failed.execution_key,
            failed.attempt,
            "dlq",
        );
        let payload = serde_json::to_vec(&failed).expect("failed job serializes");
        assert!(matches!(
            decode_replay_record(&payload),
            Ok(ReplayRecord::FailedJob(_))
        ));

        failed.event_id = "caller-selected".to_string();
        let payload = serde_json::to_vec(&failed).expect("failed job serializes");
        assert!(
            decode_replay_record(&payload)
                .expect_err("non-deterministic event id must fail")
                .contains("deterministic terminal identity")
        );
    }

    #[test]
    fn unknown_ambiguous_and_invalid_poison_records_fail_closed() {
        assert!(decode_replay_record(br#"{"kind":"future-record"}"#).is_err());

        let mut ambiguous = serde_json::to_value(sample_failed()).expect("failed job serializes");
        ambiguous["source_topic"] = serde_json::json!("pulse.scenario.jobs");
        assert!(
            decode_replay_record(
                &serde_json::to_vec(&ambiguous).expect("ambiguous record serializes")
            )
            .is_err()
        );

        let poison = PoisonMessageRecord {
            schema_version: CURRENT_CONTRACT_VERSION,
            event_id: "poison:pulse.scenario.jobs:0:9".to_string(),
            failed_at_unix_ms: 1_000,
            source_topic: "pulse.scenario.jobs".to_string(),
            source_partition: 0,
            source_offset: 8,
            source_key_base64: None,
            source_key_original_bytes: None,
            source_key_truncated: false,
            payload_base64: Some("not-base64".to_string()),
            payload_original_bytes: Some(10),
            payload_truncated: false,
            reason: "malformed".to_string(),
        };
        assert!(
            decode_replay_record(&serde_json::to_vec(&poison).expect("poison record serializes"))
                .is_err()
        );

        let mut poison_with_unknown_field =
            serde_json::to_value(poison).expect("poison record serializes");
        poison_with_unknown_field["kind"] = serde_json::json!("different-envelope");
        assert!(
            decode_replay_record(
                &serde_json::to_vec(&poison_with_unknown_field)
                    .expect("unknown envelope serializes")
            )
            .is_err()
        );
    }
}
