use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioJob {
    pub schema_version: u16,
    pub scenario_id: String,
    pub run_id: String,
    pub execution_key: String,
    pub scheduled_at_unix_ms: u128,
    pub slice: JobSlice,
    pub load: JobLoadConfig,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub max_retries: u32,
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
    pub error_breakdown: Vec<ErrorCount>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedScenarioJob {
    pub schema_version: u16,
    pub scenario_id: String,
    pub run_id: String,
    pub execution_key: String,
    pub slice: JobSlice,
    pub failed_at_unix_ms: u128,
    pub attempt: u32,
    pub max_retries: u32,
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
