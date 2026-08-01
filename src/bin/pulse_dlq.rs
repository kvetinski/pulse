use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use pulse::application::dlq_replay::{
    ReplayFilters, ReplayOptions, ReplayRecord, ReplaySource, build_replay_job,
    decode_replay_record,
};
use pulse::application::rate_limiter::TokenBucket;
use pulse::application::scenarios::load_scenarios;
use pulse::application::service::{JobPublisher, execution_semantics_fingerprint};
use pulse::infrastructure::config::AppConfig;
use pulse::infrastructure::kafka::{KafkaJobPublisher, KafkaProducerConfig};
use rdkafka::consumer::{CommitMode, Consumer, StreamConsumer};
use rdkafka::{ClientConfig, Message};
use tokio::signal;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const DEFAULT_REPLAY_LIMIT: u64 = 1_000;
const MAX_REPLAY_LIMIT: u64 = 10_000;
const MAX_REASON_SUMMARY_KEYS: usize = 64;
const MAX_REASON_SUMMARY_BYTES: usize = 160;
const REASON_SUMMARY_OVERFLOW: &str = "<other reasons>";
const REASON_TRUNCATION_MARKER: &str = "...[truncated]";
const KAFKA_RESPONSE_OVERHEAD_BYTES: usize = 512;

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn env_bool(name: &str, default: bool) -> bool {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "y" => true,
        "false" | "0" | "no" | "n" => false,
        _ => invalid_replay_env(name, "expected a boolean"),
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    raw.trim()
        .parse::<f64>()
        .unwrap_or_else(|_| invalid_replay_env(name, "expected a finite decimal number"))
}

fn env_u64(name: &str, default: u64) -> u64 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    raw.trim()
        .parse::<u64>()
        .unwrap_or_else(|_| invalid_replay_env(name, "expected a non-negative integer"))
}

fn env_u32(name: &str, default: u32) -> u32 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    raw.trim()
        .parse::<u32>()
        .unwrap_or_else(|_| invalid_replay_env(name, "expected a non-negative integer"))
}

fn env_opt_string(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn env_opt_u128(name: &str) -> Option<u128> {
    std::env::var(name).ok().map(|raw| {
        raw.trim()
            .parse::<u128>()
            .unwrap_or_else(|_| invalid_replay_env(name, "expected a non-negative integer"))
    })
}

fn invalid_replay_env<T>(name: &str, reason: &str) -> T {
    eprintln!("invalid configuration for {name}: {reason}");
    std::process::exit(2);
}

fn parse_scenario_ids(raw: Option<String>) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(list) = raw {
        for entry in list.split(',') {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                ids.insert(trimmed.to_string());
            }
        }
    }
    ids
}

fn summarize_reason(reason: &str) -> String {
    let mut summary: String = reason
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if summary.len() <= MAX_REASON_SUMMARY_BYTES {
        return summary;
    }

    let mut keep = MAX_REASON_SUMMARY_BYTES - REASON_TRUNCATION_MARKER.len();
    while !summary.is_char_boundary(keep) {
        keep -= 1;
    }
    summary.truncate(keep);
    summary.push_str(REASON_TRUNCATION_MARKER);
    summary
}

fn record_reason_summary(counts: &mut BTreeMap<String, u64>, reason: &str) {
    let reason = summarize_reason(reason);
    if let Some(count) = counts.get_mut(&reason) {
        *count = count.saturating_add(1);
        return;
    }
    let detailed_keys = counts
        .keys()
        .filter(|key| key.as_str() != REASON_SUMMARY_OVERFLOW)
        .count();
    let key = if detailed_keys < MAX_REASON_SUMMARY_KEYS {
        reason
    } else {
        REASON_SUMMARY_OVERFLOW.to_string()
    };
    let count = counts.entry(key).or_insert(0);
    *count = count.saturating_add(1);
}

fn replay_poll_budget(
    rate_per_sec: f64,
    delivery_timeout: Duration,
    request_timeout: Duration,
) -> Result<Duration, String> {
    if !rate_per_sec.is_finite() || rate_per_sec <= 0.0 {
        return Err("replay rate must be finite and greater than zero".to_string());
    }
    let pacing_delay = Duration::try_from_secs_f64(rate_per_sec.recip())
        .map_err(|_| "replay pacing interval exceeds Duration's range".to_string())?;
    pacing_delay
        .checked_add(delivery_timeout)
        .and_then(|budget| budget.checked_add(request_timeout))
        .ok_or_else(|| "replay poll budget overflowed Duration".to_string())
}

fn validate_consumed_record_size(
    source_key: Option<&[u8]>,
    payload: Option<&[u8]>,
    record_max_bytes: usize,
) -> Result<(), String> {
    let key_bytes = source_key.map_or(0, <[u8]>::len);
    let payload_bytes = payload.map_or(0, <[u8]>::len);
    let total = key_bytes
        .checked_add(payload_bytes)
        .ok_or_else(|| "consumed Kafka record byte length overflowed".to_string())?;
    if total > record_max_bytes {
        return Err(format!(
            "Kafka record exceeds configured {record_max_bytes}-byte consumer limit (total={total}, key={key_bytes}, payload={payload_bytes})"
        ));
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    init_logging();

    let app_config = AppConfig::from_env().unwrap_or_else(|err| {
        error!(error = %err, "invalid Pulse configuration");
        std::process::exit(1);
    });
    let descriptor_bytes = app_config
        .grpc_descriptor_set
        .as_deref()
        .map(|path| {
            std::fs::read(path).unwrap_or_else(|error| {
                error!(path, error = %error, "could not read gRPC descriptor set for replay fingerprint");
                std::process::exit(1);
            })
        });
    let execution_semantics_fingerprint = execution_semantics_fingerprint(
        app_config.grpc_request_timeout,
        Some(app_config.grpc_scenario_timeout),
        descriptor_bytes.as_deref(),
    );
    let scenarios = match load_scenarios(&app_config) {
        Ok(scenarios) => scenarios,
        Err(err) => {
            error!(error = %err, "failed to load scenarios");
            std::process::exit(1);
        }
    };

    let mut scenario_map = HashMap::new();
    for scenario in scenarios {
        scenario_map.insert(
            scenario.name.clone(),
            (
                scenario.clone(),
                scenario.config.partition_key_strategy.clone(),
            ),
        );
    }

    let brokers = app_config.kafka_brokers.clone();
    let dlq_topic = app_config.kafka_dlq_topic.clone();
    let jobs_topic = app_config.kafka_jobs_topic.clone();

    let group_id = env_opt_string("PULSE_DLQ_REPLAY_GROUP_ID")
        .unwrap_or_else(|| "pulse-dlq-replay".to_string());
    let dry_run = env_bool("PULSE_DLQ_REPLAY_DRY_RUN", true);
    let idempotent_ack = env_bool("PULSE_DLQ_REPLAY_CONFIRM_IDEMPOTENT", false);
    if !dry_run && !idempotent_ack {
        error!("refusing to replay without PULSE_DLQ_REPLAY_CONFIRM_IDEMPOTENT=true");
        std::process::exit(2);
    }

    let rate_per_sec = env_f64("PULSE_DLQ_REPLAY_RATE_PER_SEC", 5.0);
    if !rate_per_sec.is_finite() || rate_per_sec <= 0.0 {
        invalid_replay_env::<()>(
            "PULSE_DLQ_REPLAY_RATE_PER_SEC",
            "must be finite and greater than zero",
        );
    }
    if rate_per_sec > app_config.max_scenarios_per_sec {
        invalid_replay_env::<()>(
            "PULSE_DLQ_REPLAY_RATE_PER_SEC",
            "must not exceed PULSE_MAX_SCENARIOS_PER_SEC",
        );
    }
    let poll_budget = replay_poll_budget(
        rate_per_sec,
        app_config.kafka_delivery_timeout,
        app_config.kafka_request_timeout,
    )
    .unwrap_or_else(|reason| invalid_replay_env("PULSE_DLQ_REPLAY_RATE_PER_SEC", &reason));
    if app_config.kafka_max_poll_interval <= poll_budget {
        invalid_replay_env::<()>(
            "PULSE_DLQ_REPLAY_RATE_PER_SEC",
            &format!(
                "pacing plus publish and commit can occupy {} ms, which must be less than PULSE_KAFKA_MAX_POLL_INTERVAL_MS ({})",
                poll_budget.as_millis(),
                app_config.kafka_max_poll_interval.as_millis(),
            ),
        );
    }

    let scale = env_f64("PULSE_DLQ_REPLAY_SCALE", 1.0);
    if scale != 1.0 {
        invalid_replay_env::<()>(
            "PULSE_DLQ_REPLAY_SCALE",
            "arbitrary load scaling is no longer accepted because it violates the v2 deterministic plan contract; use 1.0 and bound replay record pacing with PULSE_DLQ_REPLAY_RATE_PER_SEC",
        );
    }

    let limit_raw = env_u64("PULSE_DLQ_REPLAY_LIMIT", DEFAULT_REPLAY_LIMIT);
    if limit_raw == 0 || limit_raw > MAX_REPLAY_LIMIT {
        invalid_replay_env::<()>(
            "PULSE_DLQ_REPLAY_LIMIT",
            "must be between 1 and 10000 records",
        );
    }
    let limit = usize::try_from(limit_raw).unwrap_or_else(|_| {
        invalid_replay_env("PULSE_DLQ_REPLAY_LIMIT", "does not fit this platform")
    });

    let options = ReplayOptions {
        rate_per_sec,
        scale,
        worker_max_retries: env_u32("PULSE_WORKER_MAX_RETRIES", 2),
        execution_semantics_fingerprint,
        dry_run,
        idempotent_ack,
    };

    let filters = ReplayFilters {
        scenario_ids: parse_scenario_ids(env_opt_string("PULSE_DLQ_REPLAY_SCENARIO_IDS")),
        reason_contains: env_opt_string("PULSE_DLQ_REPLAY_REASON_CONTAINS"),
        since_unix_ms: env_opt_u128("PULSE_DLQ_REPLAY_SINCE_UNIX_MS"),
        until_unix_ms: env_opt_u128("PULSE_DLQ_REPLAY_UNTIL_UNIX_MS"),
        limit: Some(limit),
    };

    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &brokers)
        .set("group.id", &group_id)
        .set("enable.auto.commit", "false")
        .set("enable.auto.offset.store", "false")
        .set("enable.partition.eof", "false")
        .set("auto.offset.reset", "earliest")
        .set(
            "max.poll.interval.ms",
            app_config.kafka_max_poll_interval.as_millis().to_string(),
        )
        .set(
            "session.timeout.ms",
            app_config.kafka_session_timeout.as_millis().to_string(),
        )
        .set(
            "socket.timeout.ms",
            app_config.kafka_request_timeout.as_millis().to_string(),
        )
        .set(
            "queued.max.messages.kbytes",
            app_config.consumer_queue_kbytes.to_string(),
        )
        .set(
            "fetch.message.max.bytes",
            app_config.consumer_partition_fetch_max_bytes.to_string(),
        )
        .set(
            "fetch.max.bytes",
            app_config.consumer_fetch_max_bytes.to_string(),
        )
        .set(
            "receive.message.max.bytes",
            app_config
                .consumer_fetch_max_bytes
                .saturating_add(KAFKA_RESPONSE_OVERHEAD_BYTES)
                .to_string(),
        )
        .create()
        .unwrap_or_else(|err| {
            error!(error = %err, "failed to create kafka consumer");
            std::process::exit(1);
        });

    consumer.subscribe(&[&dlq_topic]).unwrap_or_else(|err| {
        error!(error = %err, "failed to subscribe to dlq topic");
        std::process::exit(1);
    });

    let publisher = KafkaJobPublisher::new_with_config(
        &brokers,
        &jobs_topic,
        KafkaProducerConfig {
            queue_capacity_messages: app_config.producer_queue_messages,
            message_max_bytes: app_config.kafka_producer_message_max_bytes,
            message_timeout: app_config.kafka_message_timeout,
            delivery_timeout: app_config.kafka_delivery_timeout,
            request_timeout: app_config.kafka_request_timeout,
            acks: app_config.kafka_producer_acks.clone(),
            enable_idempotence: app_config.kafka_producer_idempotence,
        },
    )
    .unwrap_or_else(|err| {
        error!(error = %err, "failed to create kafka job publisher");
        std::process::exit(1);
    });

    let mut token_bucket = TokenBucket::new(rate_per_sec);
    let mut seen = 0_u64;
    let mut matched = 0_u64;
    let mut skipped_poison = 0_u64;
    let mut skipped_unknown = 0_u64;
    let mut per_scenario: BTreeMap<String, u64> = BTreeMap::new();
    let mut per_reason: BTreeMap<String, u64> = BTreeMap::new();

    info!(
        dlq_topic = %dlq_topic,
        jobs_topic = %jobs_topic,
        dry_run,
        rate_per_sec,
        scale,
        scan_limit = limit,
        "starting dlq replay"
    );

    loop {
        if seen as usize >= limit {
            info!(limit, "dlq replay scan limit reached");
            break;
        }

        tokio::select! {
            _ = signal::ctrl_c() => {
                info!("received ctrl-c, stopping");
                break;
            }
            msg = consumer.recv() => {
                let msg = match msg {
                    Ok(msg) => msg,
                    Err(err) => {
                        error!(error = %err, "failed to receive dlq message");
                        continue;
                    }
                };

                seen = seen.saturating_add(1);
                if let Err(error) = validate_consumed_record_size(
                    msg.key(),
                    msg.payload(),
                    app_config.consumer_record_max_bytes,
                ) {
                    error!(error = %error, "oversized DLQ record; stopping with replay offset unsettled");
                    break;
                }
                let payload = match msg.payload() {
                    Some(payload) => payload,
                    None => {
                        error!("received DLQ message without payload; stopping with replay offset unsettled");
                        break;
                    }
                };

                let failed = match decode_replay_record(payload) {
                    Ok(ReplayRecord::FailedJob(job)) => job,
                    Ok(ReplayRecord::Poison(poison)) => {
                        skipped_poison = skipped_poison.saturating_add(1);
                        warn!(
                            event_id = %poison.event_id,
                            source_topic = %poison.source_topic,
                            source_partition = poison.source_partition,
                            source_offset = poison.source_offset,
                            "poison DLQ evidence is not replayable; skipping"
                        );
                        if !dry_run
                            && let Err(err) = consumer.commit_message(&msg, CommitMode::Sync)
                        {
                            error!(error = %err, "failed to synchronously commit skipped poison DLQ record; stopping");
                            break;
                        }
                        continue;
                    }
                    Err(err) => {
                        error!(error = %err, "unknown or invalid DLQ payload; stopping with replay offset unsettled");
                        break;
                    }
                };

                if !filters.matches(&failed) {
                    if !dry_run {
                        if let Err(err) = consumer.commit_message(&msg, CommitMode::Sync) {
                            error!(error = %err, "failed to synchronously commit filtered DLQ record; stopping");
                            break;
                        }
                    }
                    continue;
                }

                let Some((scenario, partition_strategy)) = scenario_map.get(&failed.scenario_id) else {
                    warn!(scenario = %failed.scenario_id, "unknown scenario in dlq; skipping");
                    skipped_unknown = skipped_unknown.saturating_add(1);
                    if dry_run {
                        continue;
                    }
                    error!("unknown scenario cannot be replayed; stopping with replay offset unsettled");
                    break;
                };

                matched = matched.saturating_add(1);
                *per_scenario.entry(failed.scenario_id.clone()).or_insert(0) += 1;
                record_reason_summary(&mut per_reason, &failed.reason);

                if dry_run {
                    continue;
                }

                token_bucket.acquire().await;
                let replay_source = match ReplaySource::new(
                    msg.topic(),
                    msg.partition(),
                    msg.offset(),
                ) {
                    Ok(source) => source,
                    Err(error) => {
                        error!(error = %error, "invalid DLQ source coordinates; stopping with replay offset unsettled");
                        break;
                    }
                };
                let replay_job = match build_replay_job(
                    &failed,
                    &replay_source,
                    scenario,
                    app_config.startup_burst,
                    &options,
                ) {
                    Ok(job) => job,
                    Err(error) => {
                        error!(error = %error, "cannot construct deterministic replay job; stopping with replay offset unsettled");
                        break;
                    }
                };
                if let Err(err) = replay_job.validate_limits(
                    app_config.max_duration,
                    app_config.max_scenarios_per_sec,
                    app_config.max_concurrency,
                ) {
                    error!(error = %err, "replay job violates configured safety ceilings; stopping with replay offset unsettled");
                    break;
                }
                let key = partition_strategy.key_for(&replay_job);
                match publisher.publish_job(&key, &replay_job).await {
                    Ok(_) => {
                        if let Err(err) = consumer.commit_message(&msg, CommitMode::Sync) {
                            error!(error = %err, "replay was published but source offset commit was not acknowledged; stopping safely");
                            break;
                        }
                        info!(
                            scenario = %failed.scenario_id,
                            execution_key = %replay_job.execution_key,
                            attempt = replay_job.attempt,
                            max_retries = replay_job.max_retries,
                            "republished dlq job"
                        );
                    }
                    Err(err) => {
                        error!(error = %err, "failed to republish DLQ job; stopping before any later offset can advance");
                        break;
                    }
                }
            }
        }
    }

    if dry_run {
        println!(
            "dlq_replay_dry_run: seen={seen} matched={matched} skipped_poison={skipped_poison} skipped_unknown={skipped_unknown}"
        );
    } else {
        println!(
            "dlq_replay: seen={seen} matched={matched} skipped_poison={skipped_poison} skipped_unknown={skipped_unknown}"
        );
    }

    if !per_reason.is_empty() {
        println!("dlq_replay_reason_counts:");
        for (reason, count) in per_reason {
            println!("  {reason}: {count}");
        }
    }

    if !per_scenario.is_empty() {
        println!("dlq_replay_scenario_counts:");
        for (scenario, count) in per_scenario {
            println!("  {scenario}: {count}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_summary_is_utf8_safe_single_line_and_byte_bounded() {
        let reason = format!("prefix\n{}", "🔥".repeat(MAX_REASON_SUMMARY_BYTES));
        let summary = summarize_reason(&reason);

        assert!(summary.len() <= MAX_REASON_SUMMARY_BYTES);
        assert!(!summary.chars().any(char::is_control));
        assert!(summary.ends_with(REASON_TRUNCATION_MARKER));
    }

    #[test]
    fn reason_summary_cardinality_is_bounded() {
        let mut counts = BTreeMap::new();
        for index in 0..100 {
            record_reason_summary(&mut counts, &format!("reason-{index}"));
        }

        assert_eq!(counts.len(), MAX_REASON_SUMMARY_KEYS + 1);
        assert_eq!(counts.get(REASON_SUMMARY_OVERFLOW), Some(&36));

        record_reason_summary(&mut counts, "reason-0");
        assert_eq!(counts.get("reason-0"), Some(&2));
    }

    #[test]
    fn replay_poll_budget_includes_pacing_publish_and_commit() {
        assert_eq!(
            replay_poll_budget(5.0, Duration::from_secs(10), Duration::from_secs(5))
                .expect("valid replay budget"),
            Duration::from_millis(15_200)
        );
    }

    #[test]
    fn replay_poll_budget_rejects_unrepresentable_pacing() {
        assert!(
            replay_poll_budget(
                f64::MIN_POSITIVE,
                Duration::from_secs(10),
                Duration::from_secs(5),
            )
            .is_err()
        );
    }

    #[test]
    fn dlq_replay_rejects_records_above_combined_key_payload_limit() {
        assert!(validate_consumed_record_size(Some(b"key"), Some(b"body"), 7).is_ok());
        let error = validate_consumed_record_size(Some(b"key"), Some(b"body"), 6)
            .expect_err("combined record must be bounded before decoding");
        assert!(error.contains("total=7"));
        assert!(error.contains("key=3"));
        assert!(error.contains("payload=4"));
    }
}
