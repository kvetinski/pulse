use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::domain::error::ContractError;

pub const CURRENT_CONTRACT_VERSION: u16 = 2;
pub const MIN_SUPPORTED_CONTRACT_VERSION: u16 = 1;
pub const MAX_CONTRACT_ID_BYTES: usize = 1_024;
pub const MAX_CONTRACT_SLICES: u32 = 4_096;
pub const MAX_CONTRACT_ATTEMPT: u32 = 32;
pub const MAX_CONTRACT_ERROR_KINDS: usize = 64;
pub const MAX_CONTRACT_ERROR_KIND_BYTES: usize = 256;
pub const MAX_CONTRACT_HISTOGRAM_BUCKETS: usize = 128;
pub const MAX_CONTRACT_EXACT_INTEGER: u64 = 9_007_199_254_740_991;
/// Maximum deterministic prefix retained for each poison source field. Raw
/// suffixes are omitted so DLQ evidence remains publishable when Kafka returns
/// an oversized first batch beyond its soft fetch settings.
pub const MAX_POISON_EVIDENCE_FIELD_BYTES: usize = 256 * 1024;

pub const LATENCY_BUCKET_BOUNDS_MS: &[u64] = &[
    1, 2, 5, 10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSlice {
    pub index: u32,
    pub total: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobLoadConfig {
    pub scenarios_per_sec: f64,
    pub duration: Duration,
    pub max_concurrency: usize,
    /// Deterministic per-slice share of the explicitly configured global
    /// startup burst. Legacy v1 jobs default to strictly paced startup.
    #[serde(default)]
    pub startup_burst: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioJob {
    pub schema_version: u16,
    pub scenario_id: String,
    pub run_id: String,
    pub execution_key: String,
    /// Fingerprint of the complete scenario plan used by the scheduler. A v2
    /// worker rejects a job when its local plan does not match this value.
    #[serde(default)]
    pub plan_fingerprint: String,
    pub scheduled_at_unix_ms: u128,
    /// Earliest wall-clock time at which a worker may claim this attempt.
    /// Initial dispatches and legacy contracts default to immediate execution.
    /// Infrastructure retries persist their backoff here before the source
    /// attempt is committed, so a restart cannot lose the deferred intent.
    #[serde(default)]
    pub not_before_unix_ms: u128,
    pub slice: JobSlice,
    pub load: JobLoadConfig,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub max_retries: u32,
}

impl ScenarioJob {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_contract_version(self.schema_version)?;
        require_non_empty("scenario_id", &self.scenario_id)?;
        require_non_empty("run_id", &self.run_id)?;
        require_non_empty("execution_key", &self.execution_key)?;
        if self.schema_version >= 2 {
            require_non_empty("plan_fingerprint", &self.plan_fingerprint)?;
        }
        validate_identity_lengths(&[
            ("scenario_id", &self.scenario_id),
            ("run_id", &self.run_id),
            ("execution_key", &self.execution_key),
            ("plan_fingerprint", &self.plan_fingerprint),
        ])?;
        validate_slice(&self.slice)?;

        if !self.load.scenarios_per_sec.is_finite() || self.load.scenarios_per_sec <= 0.0 {
            return Err(ContractError::InvalidLoad(
                "scenarios_per_sec must be finite and > 0".to_string(),
            ));
        }
        if self.load.duration.is_zero() {
            return Err(ContractError::InvalidLoad(
                "duration must be greater than zero".to_string(),
            ));
        }
        if self.load.max_concurrency == 0 {
            return Err(ContractError::InvalidLoad(
                "max_concurrency must be greater than zero".to_string(),
            ));
        }
        if self.load.startup_burst > self.load.max_concurrency {
            return Err(ContractError::InvalidLoad(format!(
                "startup_burst {} exceeds slice max_concurrency {}",
                self.load.startup_burst, self.load.max_concurrency
            )));
        }
        if self.attempt > self.max_retries {
            return Err(ContractError::InvalidAttempt {
                attempt: self.attempt,
                max_retries: self.max_retries,
            });
        }
        if self.max_retries > MAX_CONTRACT_ATTEMPT {
            return Err(ContractError::InvalidAttempt {
                attempt: self.attempt,
                max_retries: self.max_retries,
            });
        }
        if self.schema_version >= 2 && self.attempt > 0 && self.not_before_unix_ms == 0 {
            return Err(ContractError::InvalidAttempt {
                attempt: self.attempt,
                max_retries: self.max_retries,
            });
        }

        Ok(())
    }

    pub fn validate_limits(
        &self,
        max_duration: Duration,
        max_rate: f64,
        max_concurrency: usize,
    ) -> Result<(), ContractError> {
        self.validate()?;
        if self.load.duration > max_duration {
            return Err(ContractError::SafetyLimit(format!(
                "duration {:?} exceeds maximum {:?}",
                self.load.duration, max_duration
            )));
        }
        if self.load.scenarios_per_sec > max_rate {
            return Err(ContractError::SafetyLimit(format!(
                "rate {} exceeds maximum {max_rate}",
                self.load.scenarios_per_sec
            )));
        }
        if self.load.max_concurrency > max_concurrency {
            return Err(ContractError::SafetyLimit(format!(
                "concurrency {} exceeds maximum {max_concurrency}",
                self.load.max_concurrency
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PartitionKeyStrategy {
    ScenarioId,
    ExecutionKey,
}

impl PartitionKeyStrategy {
    pub fn key_for(&self, job: &ScenarioJob) -> String {
        match self {
            Self::ScenarioId => job.scenario_id.clone(),
            Self::ExecutionKey => job.execution_key.clone(),
        }
    }
}

pub fn build_execution_key(
    scenario_id: &str,
    window_start_unix_ms: u128,
    slice: &JobSlice,
) -> String {
    format!(
        "{scenario_id}:{window_start_unix_ms}:slice-{}-of-{}",
        slice.index, slice.total
    )
}

pub fn build_versioned_execution_key(
    contract_version: u16,
    scenario_id: &str,
    window_start_unix_ms: u128,
    slice: &JobSlice,
) -> String {
    format!(
        "v{contract_version}:{scenario_id}:{window_start_unix_ms}:slice-{}-of-{}",
        slice.index, slice.total
    )
}

pub fn build_run_id(
    contract_version: u16,
    scenario_id: &str,
    window_start_unix_ms: u128,
) -> String {
    format!("v{contract_version}:{scenario_id}:{window_start_unix_ms}")
}

pub fn build_terminal_event_id(execution_key: &str, attempt: u32, kind: &str) -> String {
    format!("{execution_key}:attempt-{attempt}:{kind}")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScenarioRunStatus {
    Success,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioRunResult {
    pub schema_version: u16,
    pub scenario_id: String,
    pub run_id: String,
    pub execution_key: String,
    #[serde(default)]
    pub event_id: String,
    #[serde(default)]
    pub attempt: u32,
    pub slice: JobSlice,
    pub started_at_unix_ms: u128,
    pub finished_at_unix_ms: u128,
    pub status: ScenarioRunStatus,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub scenario_latency_p50_ms: u64,
    pub scenario_latency_p95_ms: u64,
    pub scenario_latency_p99_ms: u64,
    #[serde(default)]
    pub latency_histogram: Vec<LatencyBucket>,
    pub error_breakdown: Vec<ErrorCount>,
}

impl ScenarioRunResult {
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_contract_version(self.schema_version)?;
        require_non_empty("scenario_id", &self.scenario_id)?;
        require_non_empty("run_id", &self.run_id)?;
        require_non_empty("execution_key", &self.execution_key)?;
        validate_identity_lengths(&[
            ("scenario_id", &self.scenario_id),
            ("run_id", &self.run_id),
            ("execution_key", &self.execution_key),
        ])?;
        validate_slice(&self.slice)?;
        if self.attempt > MAX_CONTRACT_ATTEMPT {
            return Err(ContractError::InvalidResult(format!(
                "attempt {} exceeds maximum {MAX_CONTRACT_ATTEMPT}",
                self.attempt
            )));
        }
        if self.schema_version >= 2 {
            require_non_empty("event_id", &self.event_id)?;
            if self.event_id.len() > MAX_CONTRACT_ID_BYTES {
                return Err(ContractError::InvalidResult(format!(
                    "event_id exceeds {MAX_CONTRACT_ID_BYTES} bytes"
                )));
            }
            let expected_event_id =
                build_terminal_event_id(&self.execution_key, self.attempt, "result");
            if self.event_id != expected_event_id {
                return Err(ContractError::InvalidResult(
                    "event_id is not the deterministic result identity".to_string(),
                ));
            }
            if self.total > 0 && self.latency_histogram.is_empty() {
                return Err(ContractError::InvalidResult(
                    "version 2 results with observations require a mergeable latency histogram"
                        .to_string(),
                ));
            }
        }
        let outcome_total = self.success.checked_add(self.failure).ok_or_else(|| {
            ContractError::InvalidResult("success + failure overflowed u64".to_string())
        })?;
        if outcome_total != self.total {
            return Err(ContractError::InvalidResult(
                "success + failure must equal total".to_string(),
            ));
        }
        if self.error_breakdown.len() > MAX_CONTRACT_ERROR_KINDS {
            return Err(ContractError::InvalidResult(format!(
                "error breakdown has {} kinds; maximum is {MAX_CONTRACT_ERROR_KINDS}",
                self.error_breakdown.len()
            )));
        }
        let mut error_kinds = HashSet::with_capacity(self.error_breakdown.len());
        let mut error_total = 0_u64;
        for error in &self.error_breakdown {
            require_non_empty("error_breakdown.kind", &error.kind)?;
            if error.kind.len() > MAX_CONTRACT_ERROR_KIND_BYTES {
                return Err(ContractError::InvalidResult(format!(
                    "error kind exceeds {MAX_CONTRACT_ERROR_KIND_BYTES} bytes"
                )));
            }
            if !error_kinds.insert(error.kind.as_str()) {
                return Err(ContractError::InvalidResult(format!(
                    "duplicate error kind '{}'",
                    error.kind
                )));
            }
            error_total = error_total.checked_add(error.count).ok_or_else(|| {
                ContractError::InvalidResult("error counts overflowed u64".to_string())
            })?;
        }
        if self.schema_version >= 2 {
            for (field, value) in [
                ("total", self.total),
                ("success", self.success),
                ("failure", self.failure),
                ("error count", error_total),
            ] {
                if value > MAX_CONTRACT_EXACT_INTEGER {
                    return Err(ContractError::InvalidResult(format!(
                        "{field} exceeds the exact aggregation integer range"
                    )));
                }
            }
            if error_total != self.failure {
                return Err(ContractError::InvalidResult(format!(
                    "error count total {error_total} does not equal failure count {}",
                    self.failure
                )));
            }
            if self
                .latency_histogram
                .iter()
                .any(|bucket| bucket.count > MAX_CONTRACT_EXACT_INTEGER)
            {
                return Err(ContractError::InvalidResult(
                    "latency bucket count exceeds the exact aggregation integer range".to_string(),
                ));
            }
        } else if error_total > self.failure {
            return Err(ContractError::InvalidResult(format!(
                "error count total {error_total} exceeds failure count {}",
                self.failure
            )));
        }
        if self.finished_at_unix_ms < self.started_at_unix_ms {
            return Err(ContractError::InvalidResult(
                "finished timestamp precedes started timestamp".to_string(),
            ));
        }
        validate_latency_buckets(&self.latency_histogram, self.total)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LatencyBucket {
    /// Inclusive upper bound in milliseconds. `u64::MAX` is the overflow bucket.
    pub upper_bound_ms: u64,
    /// Non-cumulative number of observations in this bucket.
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScenarioRunSummaryStatus {
    Complete,
    Partial,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioRunSummary {
    pub schema_version: u16,
    pub scenario_id: String,
    pub run_id: String,
    pub status: ScenarioRunSummaryStatus,
    pub expected_slices: u32,
    pub received_slices: u32,
    pub missing_slices: Vec<u32>,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub scenario_latency_p50_ms: u64,
    pub scenario_latency_p95_ms: u64,
    pub scenario_latency_p99_ms: u64,
    pub latency_histogram: Vec<LatencyBucket>,
    pub error_breakdown: Vec<ErrorCount>,
    pub first_result_at_unix_ms: u128,
    pub finalized_at_unix_ms: u128,
}

/// Kafka envelope for a finalized run summary. A run can emit more than one
/// revision (for example, a timed-out summary followed by a late complete
/// summary), so consumers deduplicate by `event_id` rather than `run_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioRunSummaryEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub revision: u64,
    pub summary: ScenarioRunSummary,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedScenarioJob {
    pub schema_version: u16,
    /// Deterministic terminal-event identity for duplicate-tolerant DLQ
    /// consumers. Legacy v1 records may omit it.
    #[serde(default)]
    pub event_id: String,
    pub scenario_id: String,
    pub run_id: String,
    pub execution_key: String,
    pub slice: JobSlice,
    pub failed_at_unix_ms: u128,
    pub attempt: u32,
    pub max_retries: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PoisonMessageRecord {
    pub schema_version: u16,
    pub event_id: String,
    pub failed_at_unix_ms: u128,
    pub source_topic: String,
    pub source_partition: i32,
    pub source_offset: i64,
    pub source_key_base64: Option<String>,
    #[serde(default)]
    pub source_key_original_bytes: Option<u64>,
    #[serde(default)]
    pub source_key_truncated: bool,
    pub payload_base64: Option<String>,
    #[serde(default)]
    pub payload_original_bytes: Option<u64>,
    #[serde(default)]
    pub payload_truncated: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ErrorCount {
    pub kind: String,
    pub count: u64,
}

pub fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time must be after unix epoch")
        .as_millis()
}

pub fn validate_contract_version(version: u16) -> Result<(), ContractError> {
    if version < MIN_SUPPORTED_CONTRACT_VERSION {
        return Err(ContractError::UnsupportedVersion {
            found: version,
            min_supported: MIN_SUPPORTED_CONTRACT_VERSION,
            max_supported: CURRENT_CONTRACT_VERSION,
        });
    }
    if version > CURRENT_CONTRACT_VERSION {
        return Err(ContractError::FutureVersion {
            found: version,
            max_supported: CURRENT_CONTRACT_VERSION,
        });
    }
    Ok(())
}

pub fn validate_slice(slice: &JobSlice) -> Result<(), ContractError> {
    if slice.total == 0 {
        return Err(ContractError::InvalidSlice(
            "slice total must be greater than zero".to_string(),
        ));
    }
    if slice.total > MAX_CONTRACT_SLICES {
        return Err(ContractError::InvalidSlice(format!(
            "slice total {} exceeds maximum {MAX_CONTRACT_SLICES}",
            slice.total
        )));
    }
    if slice.index >= slice.total {
        return Err(ContractError::InvalidSlice(format!(
            "slice index {} must be less than total {}",
            slice.index, slice.total
        )));
    }
    Ok(())
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), ContractError> {
    if value.trim().is_empty() {
        return Err(ContractError::MissingField(field));
    }
    Ok(())
}

fn validate_identity_lengths(fields: &[(&'static str, &str)]) -> Result<(), ContractError> {
    for (field, value) in fields {
        if value.len() > MAX_CONTRACT_ID_BYTES {
            return Err(ContractError::Malformed(format!(
                "{field} exceeds {MAX_CONTRACT_ID_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

fn validate_latency_buckets(
    buckets: &[LatencyBucket],
    expected_total: u64,
) -> Result<(), ContractError> {
    if buckets.len() > MAX_CONTRACT_HISTOGRAM_BUCKETS {
        return Err(ContractError::InvalidResult(format!(
            "latency histogram has {} buckets; maximum is {MAX_CONTRACT_HISTOGRAM_BUCKETS}",
            buckets.len()
        )));
    }
    if buckets.is_empty() {
        // Version 1 records did not carry mergeable histograms.
        return Ok(());
    }
    let mut previous = None;
    let mut count = 0_u64;
    for bucket in buckets {
        if previous.is_some_and(|bound| bucket.upper_bound_ms <= bound) {
            return Err(ContractError::InvalidResult(
                "latency histogram bounds must be strictly increasing".to_string(),
            ));
        }
        previous = Some(bucket.upper_bound_ms);
        count = count.checked_add(bucket.count).ok_or_else(|| {
            ContractError::InvalidResult("latency histogram count overflowed u64".to_string())
        })?;
    }
    if count != expected_total {
        return Err(ContractError::InvalidResult(format!(
            "latency histogram count {count} does not equal total {expected_total}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_job() -> ScenarioJob {
        ScenarioJob {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "scenario".to_string(),
            run_id: "run".to_string(),
            execution_key: "execution".to_string(),
            plan_fingerprint: "fnv128:test-plan".to_string(),
            scheduled_at_unix_ms: 1,
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

    #[test]
    fn supports_current_and_legacy_contract_versions() {
        let mut job = valid_job();
        assert!(job.validate().is_ok());
        job.schema_version = 1;
        assert!(job.validate().is_ok());
    }

    #[test]
    fn rejects_unknown_future_contract_version() {
        let mut job = valid_job();
        job.schema_version = CURRENT_CONTRACT_VERSION + 1;
        assert!(matches!(
            job.validate(),
            Err(ContractError::FutureVersion { .. })
        ));
    }

    #[test]
    fn rejects_malformed_load_slice_and_attempt() {
        let mut job = valid_job();
        job.load.scenarios_per_sec = f64::NAN;
        assert!(matches!(job.validate(), Err(ContractError::InvalidLoad(_))));

        let mut job = valid_job();
        job.slice = JobSlice { index: 1, total: 1 };
        assert!(matches!(
            job.validate(),
            Err(ContractError::InvalidSlice(_))
        ));

        let mut job = valid_job();
        job.attempt = 3;
        assert!(matches!(
            job.validate(),
            Err(ContractError::InvalidAttempt { .. })
        ));
    }

    #[test]
    fn versioned_identities_are_deterministic() {
        let slice = JobSlice { index: 2, total: 4 };
        let left = build_versioned_execution_key(2, "checkout", 123, &slice);
        let right = build_versioned_execution_key(2, "checkout", 123, &slice);
        assert_eq!(left, right);
        assert!(left.contains("v2:checkout:123:slice-2-of-4"));
    }
}
