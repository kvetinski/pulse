use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::contracts::{
    ErrorCount, LatencyBucket, ScenarioRunResult, ScenarioRunSummary, ScenarioRunSummaryStatus,
};
use crate::domain::error::ContractError;

/// Outcome of atomically merging one slice into a durable distributed run.
#[derive(Clone, Debug)]
pub enum RunAggregationUpdate {
    Accepted {
        received_slices: u32,
        expected_slices: u32,
    },
    /// The run had already been marked partial or timed out; this late slice was retained.
    LateAccepted {
        received_slices: u32,
        expected_slices: u32,
    },
    Duplicate {
        received_slices: u32,
        expected_slices: u32,
        finalized_status: Option<ScenarioRunSummaryStatus>,
    },
    Completed(ScenarioRunSummary),
    /// A previously partial or timed-out run became complete after late slices arrived.
    LateCompleted(ScenarioRunSummary),
}

#[derive(Clone, Debug)]
pub enum RunExpiryOutcome {
    Missing,
    NotExpired { retry_after: Duration },
    MarkedTimedOut(ScenarioRunSummary),
    AlreadyFinalized(ScenarioRunSummary),
}

#[derive(Clone, Debug)]
pub enum RunFinalizationOutcome {
    Missing,
    Finalized(ScenarioRunSummary),
    AlreadyFinalized(ScenarioRunSummary),
}

#[derive(Clone, Debug)]
pub struct DurableRunSummary {
    pub revision: u64,
    pub event_id: String,
    pub pending_publication: bool,
    pub summary: ScenarioRunSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SummaryAcknowledgement {
    Acknowledged,
    AlreadyAcknowledged,
    /// An acknowledgement for an older summary must not clear a newer outbox entry.
    Stale {
        current_revision: u64,
    },
    Missing,
}

/// Failures from the durable aggregation boundary. Dependency failures cannot
/// be confused with duplicates, accepted results, or finalized summaries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunAggregationStoreError {
    Contract(ContractError),
    Unavailable {
        operation: &'static str,
        message: String,
    },
    Timeout {
        operation: &'static str,
    },
    InvalidState {
        operation: &'static str,
        message: String,
    },
    /// A permanent conflict between this record and an already accepted run.
    /// This is an input outcome, not a Redis dependency failure.
    InconsistentResult {
        message: String,
    },
    ErrorKindCapacity {
        max_error_kinds: usize,
    },
    ActiveRunCapacity {
        max_active_runs: usize,
    },
}

impl std::fmt::Display for RunAggregationStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "invalid result contract: {error}"),
            Self::Unavailable { operation, message } => {
                write!(
                    f,
                    "aggregation operation '{operation}' is unavailable: {message}"
                )
            }
            Self::Timeout { operation } => {
                write!(f, "aggregation operation '{operation}' timed out")
            }
            Self::InvalidState { operation, message } => {
                write!(
                    f,
                    "aggregation operation '{operation}' found invalid state: {message}"
                )
            }
            Self::InconsistentResult { message } => {
                write!(f, "inconsistent aggregate result: {message}")
            }
            Self::ErrorKindCapacity { max_error_kinds } => write!(
                f,
                "aggregation error-kind capacity of {max_error_kinds} was exceeded"
            ),
            Self::ActiveRunCapacity { max_active_runs } => write!(
                f,
                "aggregation active-run capacity of {max_active_runs} was exhausted"
            ),
        }
    }
}

impl std::error::Error for RunAggregationStoreError {}

impl From<ContractError> for RunAggregationStoreError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

#[async_trait]
pub trait RunAggregationStore: Send + Sync {
    /// Atomically validates and merges a slice result. Repeated execution or
    /// slice identities are duplicate outcomes, never dependency successes.
    /// `received_at_unix_ms` is a clock observation for in-process stores;
    /// durable stores may use their own authoritative clock.
    async fn ingest(
        &self,
        result: &ScenarioRunResult,
        received_at_unix_ms: u128,
    ) -> Result<RunAggregationUpdate, RunAggregationStoreError>;

    /// Returns at most `limit` run identities whose durable deadline is due.
    /// Implementations must reject zero or unbounded limits and may ignore the
    /// caller clock in favor of an authoritative coordination-store clock.
    async fn due_runs(
        &self,
        now_unix_ms: u128,
        limit: usize,
    ) -> Result<Vec<String>, RunAggregationStoreError>;

    /// Atomically marks an incomplete run timed out once its persisted deadline
    /// elapses. Late slices remain admissible. Durable implementations must
    /// compare against the same authoritative clock that created the deadline.
    async fn mark_expired(
        &self,
        run_id: &str,
        now_unix_ms: u128,
    ) -> Result<RunExpiryOutcome, RunAggregationStoreError>;

    /// Explicitly finalize an open run as partial or cancelled. Deadline-driven
    /// expiration is represented separately as `TimedOut`; completing slices
    /// are always finalized automatically as `Complete`.
    async fn finalize_run(
        &self,
        run_id: &str,
        status: ScenarioRunSummaryStatus,
        now_unix_ms: u128,
    ) -> Result<RunFinalizationOutcome, RunAggregationStoreError>;

    /// Load the latest finalized revision, whether or not Kafka has already
    /// acknowledged its publication. Open or absent runs return `None`.
    async fn load_summary(
        &self,
        run_id: &str,
    ) -> Result<Option<DurableRunSummary>, RunAggregationStoreError>;

    /// Read a bounded batch from the durable summary-publication outbox.
    async fn pending_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<DurableRunSummary>, RunAggregationStoreError>;

    /// Acknowledge publication of exactly one summary revision. A stale
    /// acknowledgement can never clear a newer partial/complete revision.
    async fn acknowledge_summary(
        &self,
        run_id: &str,
        revision: u64,
    ) -> Result<SummaryAcknowledgement, RunAggregationStoreError>;
}

#[derive(Clone, Debug)]
pub enum AggregationUpdate {
    Accepted,
    Duplicate,
    Finalized(ScenarioRunSummary),
    LateUpdate(ScenarioRunSummary),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AggregationError {
    Contract(ContractError),
    Capacity { max_runs: usize },
    InconsistentRun(String),
}

impl std::fmt::Display for AggregationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => write!(f, "invalid result contract: {error}"),
            Self::Capacity { max_runs } => {
                write!(
                    f,
                    "aggregation capacity of {max_runs} active runs is exhausted"
                )
            }
            Self::InconsistentRun(message) => write!(f, "inconsistent run result: {message}"),
        }
    }
}

impl std::error::Error for AggregationError {}

impl From<ContractError> for AggregationError {
    fn from(value: ContractError) -> Self {
        Self::Contract(value)
    }
}

pub struct RunAggregator {
    runs: HashMap<String, RunState>,
    max_runs: usize,
}

struct RunState {
    schema_version: u16,
    scenario_id: String,
    run_id: String,
    expected_slices: u32,
    received_execution_keys: HashSet<String>,
    received_slice_indexes: HashSet<u32>,
    total: u64,
    success: u64,
    failure: u64,
    histogram: BTreeMap<u64, u64>,
    errors: BTreeMap<String, u64>,
    first_result_at_unix_ms: u128,
    last_result_at_unix_ms: u128,
    finalized_status: Option<ScenarioRunSummaryStatus>,
}

impl RunAggregator {
    pub fn new(max_runs: usize) -> Self {
        assert!(max_runs > 0, "max_runs must be greater than zero");
        Self {
            runs: HashMap::new(),
            max_runs,
        }
    }

    pub fn active_runs(&self) -> usize {
        self.runs.len()
    }

    pub fn ingest(
        &mut self,
        result: ScenarioRunResult,
        received_at_unix_ms: u128,
    ) -> Result<AggregationUpdate, AggregationError> {
        result.validate()?;

        if !self.runs.contains_key(&result.run_id) && self.runs.len() >= self.max_runs {
            return Err(AggregationError::Capacity {
                max_runs: self.max_runs,
            });
        }

        let state = self
            .runs
            .entry(result.run_id.clone())
            .or_insert_with(|| RunState::from_first(&result, received_at_unix_ms));
        state.validate_compatible(&result)?;

        if state
            .received_execution_keys
            .contains(&result.execution_key)
            || state.received_slice_indexes.contains(&result.slice.index)
        {
            return Ok(AggregationUpdate::Duplicate);
        }

        let was_finalized = state.finalized_status.is_some();
        state.merge(result, received_at_unix_ms)?;
        if state.received_slice_indexes.len() == state.expected_slices as usize {
            state.finalized_status = Some(ScenarioRunSummaryStatus::Complete);
            let summary = state.summary(ScenarioRunSummaryStatus::Complete, received_at_unix_ms);
            return Ok(if was_finalized {
                AggregationUpdate::LateUpdate(summary)
            } else {
                AggregationUpdate::Finalized(summary)
            });
        }

        Ok(AggregationUpdate::Accepted)
    }

    pub fn finalize_expired(
        &mut self,
        now_unix_ms: u128,
        timeout: Duration,
    ) -> Vec<ScenarioRunSummary> {
        let timeout_ms = timeout.as_millis();
        let mut summaries = Vec::new();
        for state in self.runs.values_mut() {
            if state.finalized_status.is_some()
                || now_unix_ms.saturating_sub(state.first_result_at_unix_ms) < timeout_ms
            {
                continue;
            }
            let status = ScenarioRunSummaryStatus::TimedOut;
            state.finalized_status = Some(status.clone());
            summaries.push(state.summary(status, now_unix_ms));
        }
        summaries.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        summaries
    }

    pub fn cancel(&mut self, run_id: &str, now_unix_ms: u128) -> Option<ScenarioRunSummary> {
        let state = self.runs.get_mut(run_id)?;
        state.finalized_status = Some(ScenarioRunSummaryStatus::Cancelled);
        Some(state.summary(ScenarioRunSummaryStatus::Cancelled, now_unix_ms))
    }

    pub fn remove(&mut self, run_id: &str) -> Option<ScenarioRunSummary> {
        let state = self.runs.remove(run_id)?;
        let status = state
            .finalized_status
            .clone()
            .unwrap_or(ScenarioRunSummaryStatus::Partial);
        Some(state.summary(status, state.last_result_at_unix_ms))
    }
}

impl RunState {
    fn from_first(result: &ScenarioRunResult, now_unix_ms: u128) -> Self {
        Self {
            schema_version: result.schema_version,
            scenario_id: result.scenario_id.clone(),
            run_id: result.run_id.clone(),
            expected_slices: result.slice.total,
            received_execution_keys: HashSet::new(),
            received_slice_indexes: HashSet::new(),
            total: 0,
            success: 0,
            failure: 0,
            histogram: BTreeMap::new(),
            errors: BTreeMap::new(),
            first_result_at_unix_ms: now_unix_ms,
            last_result_at_unix_ms: now_unix_ms,
            finalized_status: None,
        }
    }

    fn validate_compatible(&self, result: &ScenarioRunResult) -> Result<(), AggregationError> {
        if self.scenario_id != result.scenario_id
            || self.expected_slices != result.slice.total
            || self.schema_version != result.schema_version
        {
            return Err(AggregationError::InconsistentRun(format!(
                "run '{}' changed scenario, slice total, or schema version",
                result.run_id
            )));
        }
        Ok(())
    }

    fn merge(
        &mut self,
        result: ScenarioRunResult,
        received_at_unix_ms: u128,
    ) -> Result<(), AggregationError> {
        if !self.histogram.is_empty() && !result.latency_histogram.is_empty() {
            let existing_bounds: Vec<_> = self.histogram.keys().copied().collect();
            let incoming_bounds: Vec<_> = result
                .latency_histogram
                .iter()
                .map(|bucket| bucket.upper_bound_ms)
                .collect();
            if existing_bounds != incoming_bounds {
                return Err(AggregationError::InconsistentRun(
                    "latency histogram bounds changed between slices".to_string(),
                ));
            }
        }

        self.received_execution_keys
            .insert(result.execution_key.clone());
        self.received_slice_indexes.insert(result.slice.index);
        self.total = self.total.saturating_add(result.total);
        self.success = self.success.saturating_add(result.success);
        self.failure = self.failure.saturating_add(result.failure);
        for bucket in result.latency_histogram {
            *self.histogram.entry(bucket.upper_bound_ms).or_insert(0) += bucket.count;
        }
        for error in result.error_breakdown {
            *self.errors.entry(error.kind).or_insert(0) += error.count;
        }
        self.last_result_at_unix_ms = received_at_unix_ms;
        Ok(())
    }

    fn summary(
        &self,
        status: ScenarioRunSummaryStatus,
        finalized_at_unix_ms: u128,
    ) -> ScenarioRunSummary {
        let histogram: Vec<_> = self
            .histogram
            .iter()
            .map(|(upper_bound_ms, count)| LatencyBucket {
                upper_bound_ms: *upper_bound_ms,
                count: *count,
            })
            .collect();
        let error_breakdown = self
            .errors
            .iter()
            .map(|(kind, count)| ErrorCount {
                kind: kind.clone(),
                count: *count,
            })
            .collect();
        let missing_slices = (0..self.expected_slices)
            .filter(|index| !self.received_slice_indexes.contains(index))
            .collect();

        ScenarioRunSummary {
            schema_version: self.schema_version,
            scenario_id: self.scenario_id.clone(),
            run_id: self.run_id.clone(),
            status,
            expected_slices: self.expected_slices,
            received_slices: self.received_slice_indexes.len() as u32,
            missing_slices,
            total: self.total,
            success: self.success,
            failure: self.failure,
            scenario_latency_p50_ms: histogram_quantile(&histogram, 0.50),
            scenario_latency_p95_ms: histogram_quantile(&histogram, 0.95),
            scenario_latency_p99_ms: histogram_quantile(&histogram, 0.99),
            latency_histogram: histogram,
            error_breakdown,
            first_result_at_unix_ms: self.first_result_at_unix_ms,
            finalized_at_unix_ms,
        }
    }
}

fn histogram_quantile(histogram: &[LatencyBucket], quantile: f64) -> u64 {
    let total: u64 = histogram.iter().map(|bucket| bucket.count).sum();
    if total == 0 {
        return 0;
    }
    let rank = ((total as f64) * quantile).ceil().max(1.0) as u64;
    let mut cumulative = 0_u64;
    let mut previous_bound = 0_u64;
    for bucket in histogram {
        cumulative = cumulative.saturating_add(bucket.count);
        if cumulative >= rank {
            return if bucket.upper_bound_ms == u64::MAX {
                previous_bound
            } else {
                bucket.upper_bound_ms
            };
        }
        previous_bound = bucket.upper_bound_ms;
    }
    previous_bound
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::contracts::{
        CURRENT_CONTRACT_VERSION, JobSlice, ScenarioRunStatus, build_terminal_event_id,
    };

    fn result(index: u32, total: u32, successes: u64, latency_bound: u64) -> ScenarioRunResult {
        let execution_key = format!("execution-{index}");
        ScenarioRunResult {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run-1".to_string(),
            event_id: build_terminal_event_id(&execution_key, 0, "result"),
            attempt: 0,
            execution_key,
            slice: JobSlice { index, total },
            started_at_unix_ms: 1,
            finished_at_unix_ms: 2,
            status: ScenarioRunStatus::Success,
            total: successes,
            success: successes,
            failure: 0,
            scenario_latency_p50_ms: latency_bound,
            scenario_latency_p95_ms: latency_bound,
            scenario_latency_p99_ms: latency_bound,
            latency_histogram: vec![
                LatencyBucket {
                    upper_bound_ms: 10,
                    count: if latency_bound <= 10 { successes } else { 0 },
                },
                LatencyBucket {
                    upper_bound_ms: 100,
                    count: if latency_bound > 10 { successes } else { 0 },
                },
            ],
            error_breakdown: Vec::new(),
        }
    }

    #[test]
    fn duplicate_and_out_of_order_results_finalize_once_without_double_counting() {
        let mut aggregator = RunAggregator::new(10);
        assert!(matches!(
            aggregator.ingest(result(1, 2, 3, 100), 10),
            Ok(AggregationUpdate::Accepted)
        ));
        assert!(matches!(
            aggregator.ingest(result(1, 2, 3, 100), 11),
            Ok(AggregationUpdate::Duplicate)
        ));
        let update = aggregator.ingest(result(0, 2, 7, 10), 12).unwrap();
        let AggregationUpdate::Finalized(summary) = update else {
            panic!("expected complete summary");
        };
        assert_eq!(summary.total, 10);
        assert_eq!(summary.received_slices, 2);
        assert!(summary.missing_slices.is_empty());
        assert_eq!(summary.scenario_latency_p50_ms, 10);
        assert_eq!(summary.scenario_latency_p95_ms, 100);
    }

    #[test]
    fn missing_slice_times_out_and_late_result_completes_it() {
        let mut aggregator = RunAggregator::new(10);
        aggregator.ingest(result(0, 2, 2, 10), 100).unwrap();
        let summaries = aggregator.finalize_expired(201, Duration::from_millis(100));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].status, ScenarioRunSummaryStatus::TimedOut);
        assert_eq!(summaries[0].missing_slices, vec![1]);

        let update = aggregator.ingest(result(1, 2, 1, 100), 220).unwrap();
        let AggregationUpdate::LateUpdate(summary) = update else {
            panic!("expected late completion update");
        };
        assert_eq!(summary.status, ScenarioRunSummaryStatus::Complete);
        assert_eq!(summary.total, 3);
    }

    #[test]
    fn active_run_capacity_is_bounded() {
        let mut aggregator = RunAggregator::new(1);
        aggregator.ingest(result(0, 2, 1, 10), 1).unwrap();
        let mut other = result(0, 2, 1, 10);
        other.run_id = "run-2".to_string();
        assert!(matches!(
            aggregator.ingest(other, 2),
            Err(AggregationError::Capacity { max_runs: 1 })
        ));
    }
}
