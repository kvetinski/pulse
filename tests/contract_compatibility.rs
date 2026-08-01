use std::time::Duration;

use pulse::domain::contracts::{
    CURRENT_CONTRACT_VERSION, ErrorCount, JobLoadConfig, JobSlice, LatencyBucket,
    MAX_CONTRACT_ERROR_KIND_BYTES, MAX_CONTRACT_EXACT_INTEGER, PoisonMessageRecord, ScenarioJob,
    ScenarioRunResult, ScenarioRunStatus,
};
use pulse::domain::error::ContractError;
use serde_json::Value;

fn valid_job() -> ScenarioJob {
    ScenarioJob {
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: "contract-test".to_string(),
        run_id: "run-1".to_string(),
        execution_key: "run-1:slice-0-of-1".to_string(),
        plan_fingerprint: "fnv128:contract-test-plan".to_string(),
        scheduled_at_unix_ms: 123,
        not_before_unix_ms: 0,
        slice: JobSlice { index: 0, total: 1 },
        load: JobLoadConfig {
            scenarios_per_sec: 0.1,
            duration: Duration::from_secs(1),
            max_concurrency: 1,
            startup_burst: 0,
        },
        attempt: 0,
        max_retries: 2,
    }
}

fn valid_result() -> ScenarioRunResult {
    ScenarioRunResult {
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: "contract-test".to_string(),
        run_id: "run-1".to_string(),
        execution_key: "run-1:slice-0-of-1".to_string(),
        event_id: "run-1:slice-0-of-1:attempt-0:result".to_string(),
        attempt: 0,
        slice: JobSlice { index: 0, total: 1 },
        started_at_unix_ms: 100,
        finished_at_unix_ms: 110,
        status: ScenarioRunStatus::Success,
        total: 2,
        success: 2,
        failure: 0,
        scenario_latency_p50_ms: 10,
        scenario_latency_p95_ms: 25,
        scenario_latency_p99_ms: 25,
        latency_histogram: vec![
            LatencyBucket {
                upper_bound_ms: 10,
                count: 1,
            },
            LatencyBucket {
                upper_bound_ms: 25,
                count: 1,
            },
        ],
        error_breakdown: Vec::new(),
    }
}

#[test]
fn legacy_v1_job_defaults_attempt_fields_and_remains_supported() {
    let mut value = serde_json::to_value(valid_job()).expect("job serializes");
    let Value::Object(ref mut object) = value else {
        panic!("job must serialize as an object");
    };
    object.insert("schema_version".to_string(), Value::from(1));
    object.remove("attempt");
    object.remove("max_retries");
    object.remove("not_before_unix_ms");
    object.remove("plan_fingerprint");
    object
        .get_mut("load")
        .and_then(Value::as_object_mut)
        .expect("load is an object")
        .remove("startup_burst");

    let decoded: ScenarioJob = serde_json::from_value(value).expect("legacy job decodes");
    assert_eq!(decoded.attempt, 0);
    assert_eq!(decoded.max_retries, 0);
    assert_eq!(decoded.not_before_unix_ms, 0);
    assert!(decoded.plan_fingerprint.is_empty());
    assert_eq!(decoded.load.startup_burst, 0);
    assert!(decoded.validate().is_ok());
}

#[test]
fn unknown_future_contract_is_not_silently_accepted() {
    let mut job = valid_job();
    job.schema_version = CURRENT_CONTRACT_VERSION + 1;
    assert!(matches!(
        job.validate(),
        Err(ContractError::FutureVersion {
            found,
            max_supported: CURRENT_CONTRACT_VERSION,
        }) if found == CURRENT_CONTRACT_VERSION + 1
    ));
}

#[test]
fn missing_required_job_field_fails_deserialization() {
    let mut value = serde_json::to_value(valid_job()).expect("job serializes");
    let Value::Object(ref mut object) = value else {
        panic!("job must serialize as an object");
    };
    object.remove("scenario_id");

    assert!(serde_json::from_value::<ScenarioJob>(value).is_err());
}

#[test]
fn malformed_load_slice_and_attempt_have_distinct_contract_errors() {
    let mut job = valid_job();
    job.load.scenarios_per_sec = 0.0;
    assert!(matches!(job.validate(), Err(ContractError::InvalidLoad(_))));

    let mut job = valid_job();
    job.slice = JobSlice { index: 2, total: 2 };
    assert!(matches!(
        job.validate(),
        Err(ContractError::InvalidSlice(_))
    ));

    let mut job = valid_job();
    job.attempt = 3;
    job.max_retries = 2;
    assert!(matches!(
        job.validate(),
        Err(ContractError::InvalidAttempt {
            attempt: 3,
            max_retries: 2,
        })
    ));
}

#[test]
fn legacy_v1_result_without_mergeable_fields_decodes_explicitly() {
    let mut value = serde_json::to_value(valid_result()).expect("result serializes");
    let Value::Object(ref mut object) = value else {
        panic!("result must serialize as an object");
    };
    object.insert("schema_version".to_string(), Value::from(1));
    object.remove("event_id");
    object.remove("attempt");
    object.remove("latency_histogram");

    let decoded: ScenarioRunResult = serde_json::from_value(value).expect("legacy result decodes");
    assert!(decoded.event_id.is_empty());
    assert_eq!(decoded.attempt, 0);
    assert!(decoded.latency_histogram.is_empty());
    assert!(decoded.validate().is_ok());
}

#[test]
fn malformed_mergeable_histogram_is_rejected() {
    let mut result = valid_result();
    result.latency_histogram[1].upper_bound_ms = 10;
    assert!(matches!(
        result.validate(),
        Err(ContractError::InvalidResult(_))
    ));

    let mut result = valid_result();
    result.latency_histogram[1].count = 2;
    assert!(matches!(
        result.validate(),
        Err(ContractError::InvalidResult(_))
    ));
}

#[test]
fn version_two_requires_deterministic_event_and_mergeable_observations() {
    let mut result = valid_result();
    result.event_id.clear();
    assert!(matches!(
        result.validate(),
        Err(ContractError::MissingField("event_id"))
    ));

    let mut result = valid_result();
    result.event_id = "caller-chosen".to_string();
    assert!(matches!(
        result.validate(),
        Err(ContractError::InvalidResult(_))
    ));

    let mut result = valid_result();
    result.latency_histogram.clear();
    assert!(matches!(
        result.validate(),
        Err(ContractError::InvalidResult(_))
    ));

    let mut result = valid_result();
    result.attempt = 33;
    result.event_id = pulse::domain::contracts::build_terminal_event_id(
        &result.execution_key,
        result.attempt,
        "result",
    );
    assert!(matches!(
        result.validate(),
        Err(ContractError::InvalidResult(_))
    ));
}

#[test]
fn version_two_rejects_unbounded_or_inexact_aggregation_input() {
    let mut result = valid_result();
    result.failure = 1;
    result.success = 1;
    result.status = ScenarioRunStatus::Failed;
    result.error_breakdown = vec![ErrorCount {
        kind: "target_status:internal".to_string(),
        count: 1,
    }];
    assert!(result.validate().is_ok());

    let mut oversized_kind = result.clone();
    oversized_kind.error_breakdown[0].kind = "x".repeat(MAX_CONTRACT_ERROR_KIND_BYTES + 1);
    assert!(matches!(
        oversized_kind.validate(),
        Err(ContractError::InvalidResult(_))
    ));

    let mut mismatched_errors = result.clone();
    mismatched_errors.error_breakdown[0].count = 0;
    assert!(matches!(
        mismatched_errors.validate(),
        Err(ContractError::InvalidResult(_))
    ));

    let mut inexact = valid_result();
    inexact.total = MAX_CONTRACT_EXACT_INTEGER + 1;
    inexact.success = inexact.total;
    inexact.latency_histogram[0].count = inexact.total;
    inexact.latency_histogram[1].count = 0;
    assert!(matches!(
        inexact.validate(),
        Err(ContractError::InvalidResult(_))
    ));
}

#[test]
fn legacy_poison_evidence_defaults_to_untruncated_unknown_lengths() {
    let legacy = serde_json::json!({
        "schema_version": 1,
        "event_id": "poison:pulse.jobs:0:7",
        "failed_at_unix_ms": 100,
        "source_topic": "pulse.jobs",
        "source_partition": 0,
        "source_offset": 7,
        "source_key_base64": null,
        "payload_base64": "bm90LWpzb24=",
        "reason": "malformed"
    });
    let poison: PoisonMessageRecord =
        serde_json::from_value(legacy).expect("legacy poison envelope remains readable");

    assert_eq!(poison.payload_original_bytes, None);
    assert!(!poison.payload_truncated);
    assert_eq!(poison.source_key_original_bytes, None);
    assert!(!poison.source_key_truncated);
}
