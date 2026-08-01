use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval, timeout};
use tracing::{info, warn};

use crate::application::aggregation::{
    DurableRunSummary, RunAggregationStore, RunAggregationStoreError, RunAggregationUpdate,
    RunExpiryOutcome, SummaryAcknowledgement,
};
use crate::application::service::DlqPublisher;
use crate::domain::contracts::{
    CURRENT_CONTRACT_VERSION, PoisonMessageRecord, ScenarioRunResult, ScenarioRunSummaryEvent,
    now_unix_ms,
};
use crate::domain::error::ContractError;
use crate::infrastructure::metrics as runtime_metrics;

#[async_trait]
pub trait ResultConsumer: Send + Sync {
    type Item: CommitableResult + Send;

    async fn recv(&self) -> Result<Option<Self::Item>, String>;
}

#[async_trait]
pub trait CommitableResult: Send + Sync {
    fn result(&self) -> Result<&ScenarioRunResult, ContractError>;
    fn poison_record(&self, reason: String) -> PoisonMessageRecord;
    async fn commit(self) -> Result<(), String>;
}

#[async_trait]
pub trait SummaryPublisher: Send + Sync {
    /// Success means Kafka acknowledged this exact deterministic revision.
    async fn publish_summary(&self, event: &ScenarioRunSummaryEvent) -> Result<(), String>;
}

#[derive(Clone, Debug)]
pub struct AggregationRuntimeConfig {
    pub scan_interval: Duration,
    pub scan_batch_limit: usize,
    pub outbox_batch_limit: usize,
    pub shutdown_drain_timeout: Duration,
    /// Bounds every accepted result and maintenance cycle below Kafka's
    /// max-poll interval. Expiry is fail-stop with the source offset unsettled.
    pub max_processing_interval: Duration,
}

impl Default for AggregationRuntimeConfig {
    fn default() -> Self {
        Self {
            scan_interval: Duration::from_secs(1),
            scan_batch_limit: 128,
            outbox_batch_limit: 128,
            shutdown_drain_timeout: Duration::from_secs(10),
            max_processing_interval: Duration::from_secs(299),
        }
    }
}

pub struct RunAggregationRuntime<S, C, P, D>
where
    S: RunAggregationStore,
    C: ResultConsumer,
    P: SummaryPublisher,
    D: DlqPublisher,
{
    store: Arc<S>,
    consumer: Arc<C>,
    publisher: Arc<P>,
    dlq_publisher: Arc<D>,
    config: AggregationRuntimeConfig,
}

impl<S, C, P, D> RunAggregationRuntime<S, C, P, D>
where
    S: RunAggregationStore + 'static,
    C: ResultConsumer + 'static,
    P: SummaryPublisher + 'static,
    D: DlqPublisher + 'static,
{
    pub fn new(
        store: Arc<S>,
        consumer: Arc<C>,
        publisher: Arc<P>,
        dlq_publisher: Arc<D>,
        config: AggregationRuntimeConfig,
    ) -> Result<Self, String> {
        if config.scan_interval.is_zero() {
            return Err("aggregation scan interval must be greater than zero".to_string());
        }
        if config.scan_batch_limit == 0 || config.outbox_batch_limit == 0 {
            return Err("aggregation scan and outbox limits must be greater than zero".to_string());
        }
        if config.shutdown_drain_timeout.is_zero() {
            return Err("aggregation shutdown drain timeout must be greater than zero".to_string());
        }
        if config.max_processing_interval.is_zero() {
            return Err("aggregation processing interval must be greater than zero".to_string());
        }
        Ok(Self {
            store,
            consumer,
            publisher,
            dlq_publisher,
            config,
        })
    }

    /// Runs a single bounded consumer and a durable Redis-backed deadline/outbox
    /// scan. Any unsettled dependency failure is fail-stop: the source result
    /// offset remains uncommitted and Kafka redelivers it after restart.
    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> Result<(), String> {
        let mut scan = interval(self.config.scan_interval);
        scan.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Recover pending summaries and expired runs before accepting new input.
        self.maintenance_with_deadline("startup").await?;
        info!("distributed result aggregator ready");

        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = scan.tick() => self.maintenance_with_deadline("periodic").await?,
                record = self.consumer.recv() => {
                    match record? {
                        Some(record) => self.process_record_with_deadline(record).await?,
                        None => continue,
                    }
                }
            }
        }

        // The outbox itself is durable, but a bounded final flush reduces noisy
        // duplicate publication after an ordinary shutdown.
        match timeout(self.config.shutdown_drain_timeout, self.publish_pending()).await {
            Ok(Ok(())) => info!("aggregation outbox drained for shutdown"),
            Ok(Err(error)) => {
                warn!(error = %error, "aggregation outbox remains durable for restart")
            }
            Err(_) => {
                warn!("aggregation drain deadline expired; outbox remains durable for restart")
            }
        }
        Ok(())
    }

    async fn process_record(&self, record: C::Item) -> Result<(), String> {
        let result = match record.result() {
            Ok(result) => result,
            Err(error) => {
                return self
                    .dead_letter_and_commit(record, format!("invalid result contract: {error}"))
                    .await;
            }
        };

        match self.store.ingest(result, now_unix_ms()).await {
            Ok(update) => {
                runtime_metrics::record_aggregation_update(update_label(&update));
            }
            Err(error @ RunAggregationStoreError::Contract(_))
            | Err(error @ RunAggregationStoreError::InconsistentResult { .. })
            | Err(error @ RunAggregationStoreError::ErrorKindCapacity { .. }) => {
                // These inputs cannot become valid through retry. A durable
                // poison record is their terminal disposition.
                return self.dead_letter_and_commit(record, error.to_string()).await;
            }
            Err(error) => {
                return Err(format!(
                    "result aggregation failed before durable acceptance; source offset remains uncommitted: {error}"
                ));
            }
        }

        record.commit().await.map_err(|error| {
            format!(
                "result was durably aggregated but synchronous source commit failed; duplicate redelivery is safe: {error}"
            )
        })?;
        Ok(())
    }

    async fn process_record_with_deadline(&self, record: C::Item) -> Result<(), String> {
        timeout(self.config.max_processing_interval, self.process_record(record))
            .await
            .map_err(|_| {
                format!(
                    "result processing exceeded its {} ms Kafka-safe bound; source offset remains unsettled",
                    self.config.max_processing_interval.as_millis()
                )
            })?
    }

    async fn dead_letter_and_commit(&self, record: C::Item, reason: String) -> Result<(), String> {
        let poison = record.poison_record(reason);
        self.dlq_publisher
            .publish_poison(&poison)
            .await
            .map_err(|error| {
                format!(
                    "result poison DLQ publication failed; source offset remains uncommitted: {error}"
                )
            })?;
        record.commit().await.map_err(|error| {
            format!("result poison was published but synchronous source commit failed: {error}")
        })
    }

    async fn maintenance(&self) -> Result<(), String> {
        self.expire_due_runs().await?;
        self.publish_pending().await
    }

    async fn maintenance_with_deadline(&self, phase: &str) -> Result<(), String> {
        timeout(self.config.max_processing_interval, self.maintenance())
            .await
            .map_err(|_| {
                format!(
                    "aggregation {phase} maintenance exceeded its {} ms Kafka-safe bound",
                    self.config.max_processing_interval.as_millis()
                )
            })?
    }

    async fn expire_due_runs(&self) -> Result<(), String> {
        let now = now_unix_ms();
        let due = self
            .store
            .due_runs(now, self.config.scan_batch_limit)
            .await
            .map_err(|error| format!("aggregation deadline scan failed: {error}"))?;
        let mut tasks = tokio::task::JoinSet::new();
        for run_id in due {
            let store = self.store.clone();
            tasks.spawn(async move {
                let outcome = store.mark_expired(&run_id, now).await;
                (run_id, outcome)
            });
        }
        let mut first_error = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((_, Ok(RunExpiryOutcome::Missing | RunExpiryOutcome::NotExpired { .. }))) => {}
                Ok((_, Ok(RunExpiryOutcome::MarkedTimedOut(_)))) => {
                    runtime_metrics::record_aggregation_update("timed_out");
                }
                Ok((_, Ok(RunExpiryOutcome::AlreadyFinalized(_)))) => {}
                Ok((run_id, Err(error))) => {
                    first_error.get_or_insert_with(|| {
                        format!("failed to finalize expired aggregate '{run_id}': {error}")
                    });
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        format!("aggregation expiration task failed: {error}")
                    });
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn publish_pending(&self) -> Result<(), String> {
        let pending = self
            .store
            .pending_summaries(self.config.outbox_batch_limit)
            .await
            .map_err(|error| format!("aggregation outbox scan failed: {error}"))?;
        let mut tasks = tokio::task::JoinSet::new();
        for durable in pending {
            let store = self.store.clone();
            let publisher = self.publisher.clone();
            tasks.spawn(async move { publish_one(store, publisher, durable).await });
        }
        let mut first_error = None;
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        format!("aggregation publication task failed: {error}")
                    });
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    #[cfg(test)]
    async fn publish_one(&self, durable: DurableRunSummary) -> Result<(), String> {
        publish_one(self.store.clone(), self.publisher.clone(), durable).await
    }
}

async fn publish_one<S: RunAggregationStore, P: SummaryPublisher>(
    store: Arc<S>,
    publisher: Arc<P>,
    durable: DurableRunSummary,
) -> Result<(), String> {
    let event = ScenarioRunSummaryEvent {
        schema_version: CURRENT_CONTRACT_VERSION,
        event_id: durable.event_id.clone(),
        revision: durable.revision,
        summary: durable.summary,
    };
    publisher.publish_summary(&event).await.map_err(|error| {
        format!(
            "summary '{}' publication failed; Redis outbox remains pending: {error}",
            event.event_id
        )
    })?;
    match store
        .acknowledge_summary(&event.summary.run_id, event.revision)
        .await
        .map_err(|error| {
            format!(
                "summary '{}' was published but its Redis outbox acknowledgement failed: {error}",
                event.event_id
            )
        })? {
        SummaryAcknowledgement::Acknowledged
        | SummaryAcknowledgement::AlreadyAcknowledged
        | SummaryAcknowledgement::Stale { .. } => Ok(()),
        SummaryAcknowledgement::Missing => Err(format!(
            "summary '{}' was published but its durable run disappeared",
            event.event_id
        )),
    }
}

fn update_label(update: &RunAggregationUpdate) -> &'static str {
    match update {
        RunAggregationUpdate::Accepted { .. } => "accepted",
        RunAggregationUpdate::LateAccepted { .. } => "late",
        RunAggregationUpdate::Duplicate { .. } => "duplicate",
        RunAggregationUpdate::Completed(_) => "complete",
        RunAggregationUpdate::LateCompleted(_) => "late_complete",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::application::aggregation::{RunAggregationStoreError, RunFinalizationOutcome};
    use crate::domain::contracts::{
        FailedScenarioJob, JobSlice, LatencyBucket, ScenarioRunStatus, ScenarioRunSummary,
        ScenarioRunSummaryStatus, build_terminal_event_id,
    };

    struct FakeRecord {
        result: Result<ScenarioRunResult, ContractError>,
        commits: Arc<AtomicUsize>,
        commit_delay: Duration,
    }

    #[async_trait]
    impl CommitableResult for FakeRecord {
        fn result(&self) -> Result<&ScenarioRunResult, ContractError> {
            self.result.as_ref().map_err(Clone::clone)
        }

        fn poison_record(&self, reason: String) -> PoisonMessageRecord {
            PoisonMessageRecord {
                schema_version: CURRENT_CONTRACT_VERSION,
                event_id: "poison:results:0:1".to_string(),
                failed_at_unix_ms: 1,
                source_topic: "results".to_string(),
                source_partition: 0,
                source_offset: 1,
                source_key_base64: None,
                source_key_original_bytes: None,
                source_key_truncated: false,
                payload_base64: None,
                payload_original_bytes: None,
                payload_truncated: false,
                reason,
            }
        }

        async fn commit(self) -> Result<(), String> {
            tokio::time::sleep(self.commit_delay).await;
            self.commits.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeConsumer;

    #[async_trait]
    impl ResultConsumer for FakeConsumer {
        type Item = FakeRecord;

        async fn recv(&self) -> Result<Option<Self::Item>, String> {
            std::future::pending().await
        }
    }

    #[derive(Clone, Copy)]
    enum StoreMode {
        Accepted,
        Duplicate,
        Unavailable,
    }

    struct FakeStore {
        mode: StoreMode,
        acknowledgements: AtomicUsize,
    }

    #[async_trait]
    impl RunAggregationStore for FakeStore {
        async fn ingest(
            &self,
            _result: &ScenarioRunResult,
            _received_at_unix_ms: u128,
        ) -> Result<RunAggregationUpdate, RunAggregationStoreError> {
            match self.mode {
                StoreMode::Accepted => Ok(RunAggregationUpdate::Accepted {
                    received_slices: 1,
                    expected_slices: 2,
                }),
                StoreMode::Duplicate => Ok(RunAggregationUpdate::Duplicate {
                    received_slices: 1,
                    expected_slices: 2,
                    finalized_status: None,
                }),
                StoreMode::Unavailable => Err(RunAggregationStoreError::Unavailable {
                    operation: "ingest",
                    message: "offline".to_string(),
                }),
            }
        }

        async fn due_runs(
            &self,
            _now_unix_ms: u128,
            _limit: usize,
        ) -> Result<Vec<String>, RunAggregationStoreError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            _run_id: &str,
            _now_unix_ms: u128,
        ) -> Result<RunExpiryOutcome, RunAggregationStoreError> {
            Ok(RunExpiryOutcome::Missing)
        }

        async fn finalize_run(
            &self,
            _run_id: &str,
            _status: ScenarioRunSummaryStatus,
            _now_unix_ms: u128,
        ) -> Result<RunFinalizationOutcome, RunAggregationStoreError> {
            Ok(RunFinalizationOutcome::Missing)
        }

        async fn load_summary(
            &self,
            _run_id: &str,
        ) -> Result<Option<DurableRunSummary>, RunAggregationStoreError> {
            Ok(None)
        }

        async fn pending_summaries(
            &self,
            _limit: usize,
        ) -> Result<Vec<DurableRunSummary>, RunAggregationStoreError> {
            Ok(Vec::new())
        }

        async fn acknowledge_summary(
            &self,
            _run_id: &str,
            _revision: u64,
        ) -> Result<SummaryAcknowledgement, RunAggregationStoreError> {
            self.acknowledgements.fetch_add(1, Ordering::SeqCst);
            Ok(SummaryAcknowledgement::Acknowledged)
        }
    }

    struct FakePublisher {
        fail: AtomicBool,
    }

    #[async_trait]
    impl SummaryPublisher for FakePublisher {
        async fn publish_summary(&self, _event: &ScenarioRunSummaryEvent) -> Result<(), String> {
            if self.fail.load(Ordering::SeqCst) {
                Err("broker unavailable".to_string())
            } else {
                Ok(())
            }
        }
    }

    struct FakeDlq {
        fail: AtomicBool,
        published: AtomicUsize,
    }

    #[async_trait]
    impl DlqPublisher for FakeDlq {
        async fn publish_failed_job(
            &self,
            _key: &str,
            _job: &FailedScenarioJob,
        ) -> Result<(), String> {
            unreachable!("aggregation only publishes poison records")
        }

        async fn publish_poison(&self, _record: &PoisonMessageRecord) -> Result<(), String> {
            if self.fail.load(Ordering::SeqCst) {
                Err("DLQ unavailable".to_string())
            } else {
                self.published.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    type Runtime = RunAggregationRuntime<FakeStore, FakeConsumer, FakePublisher, FakeDlq>;

    fn runtime(mode: StoreMode, summary_fail: bool, dlq_fail: bool) -> Runtime {
        RunAggregationRuntime::new(
            Arc::new(FakeStore {
                mode,
                acknowledgements: AtomicUsize::new(0),
            }),
            Arc::new(FakeConsumer),
            Arc::new(FakePublisher {
                fail: AtomicBool::new(summary_fail),
            }),
            Arc::new(FakeDlq {
                fail: AtomicBool::new(dlq_fail),
                published: AtomicUsize::new(0),
            }),
            AggregationRuntimeConfig::default(),
        )
        .unwrap()
    }

    fn result() -> ScenarioRunResult {
        let execution_key = "run:slice-0-of-2".to_string();
        ScenarioRunResult {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run".to_string(),
            event_id: build_terminal_event_id(&execution_key, 0, "result"),
            attempt: 0,
            execution_key,
            slice: JobSlice { index: 0, total: 2 },
            started_at_unix_ms: 1,
            finished_at_unix_ms: 2,
            status: ScenarioRunStatus::Success,
            total: 1,
            success: 1,
            failure: 0,
            scenario_latency_p50_ms: 10,
            scenario_latency_p95_ms: 10,
            scenario_latency_p99_ms: 10,
            latency_histogram: vec![LatencyBucket {
                upper_bound_ms: 10,
                count: 1,
            }],
            error_breakdown: Vec::new(),
        }
    }

    fn record(commits: Arc<AtomicUsize>) -> FakeRecord {
        FakeRecord {
            result: Ok(result()),
            commits,
            commit_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn redis_outage_is_not_a_duplicate_and_does_not_commit() {
        let commits = Arc::new(AtomicUsize::new(0));
        let error = runtime(StoreMode::Unavailable, false, false)
            .process_record(record(commits.clone()))
            .await
            .unwrap_err();
        assert!(error.contains("remains uncommitted"));
        assert_eq!(commits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn durable_acceptance_and_verified_duplicate_commit() {
        for mode in [StoreMode::Accepted, StoreMode::Duplicate] {
            let commits = Arc::new(AtomicUsize::new(0));
            runtime(mode, false, false)
                .process_record(record(commits.clone()))
                .await
                .unwrap();
            assert_eq!(commits.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn malformed_result_requires_durable_poison_before_commit() {
        for (dlq_fails, expected_commits) in [(true, 0), (false, 1)] {
            let commits = Arc::new(AtomicUsize::new(0));
            let record = FakeRecord {
                result: Err(ContractError::Malformed("bad JSON".to_string())),
                commits: commits.clone(),
                commit_delay: Duration::ZERO,
            };
            let outcome = runtime(StoreMode::Accepted, false, dlq_fails)
                .process_record(record)
                .await;
            assert_eq!(outcome.is_ok(), !dlq_fails);
            assert_eq!(commits.load(Ordering::SeqCst), expected_commits);
        }
    }

    #[tokio::test]
    async fn failed_summary_publication_never_acknowledges_the_outbox() {
        let runtime = runtime(StoreMode::Accepted, true, false);
        let summary = ScenarioRunSummary {
            schema_version: CURRENT_CONTRACT_VERSION,
            scenario_id: "checkout".to_string(),
            run_id: "run".to_string(),
            status: ScenarioRunSummaryStatus::Complete,
            expected_slices: 1,
            received_slices: 1,
            missing_slices: Vec::new(),
            total: 1,
            success: 1,
            failure: 0,
            scenario_latency_p50_ms: 10,
            scenario_latency_p95_ms: 10,
            scenario_latency_p99_ms: 10,
            latency_histogram: vec![LatencyBucket {
                upper_bound_ms: 10,
                count: 1,
            }],
            error_breakdown: Vec::new(),
            first_result_at_unix_ms: 1,
            finalized_at_unix_ms: 2,
        };
        let durable = DurableRunSummary {
            revision: 1,
            event_id: "run:summary:r1:complete".to_string(),
            pending_publication: true,
            summary,
        };
        assert!(runtime.publish_one(durable).await.is_err());
        assert_eq!(runtime.store.acknowledgements.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn slow_result_commit_hits_kafka_safe_deadline_without_commit() {
        let commits = Arc::new(AtomicUsize::new(0));
        let mut runtime = runtime(StoreMode::Accepted, false, false);
        runtime.config.max_processing_interval = Duration::from_millis(25);
        let record = FakeRecord {
            result: Ok(result()),
            commits: commits.clone(),
            commit_delay: Duration::from_secs(60),
        };

        let error = runtime
            .process_record_with_deadline(record)
            .await
            .expect_err("slow commit must fail-stop before max.poll.interval");

        assert!(error.contains("Kafka-safe bound"), "{error}");
        assert_eq!(commits.load(Ordering::SeqCst), 0);
    }
}
