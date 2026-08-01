use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tracing::{error, info, warn};

use crate::application::metrics::MetricsBucket;
use crate::application::runner::{PulseRunner, RunnerConfig};
use crate::domain::contracts::{
    CURRENT_CONTRACT_VERSION, ErrorCount, FailedScenarioJob, JobLoadConfig, JobSlice,
    MAX_CONTRACT_ATTEMPT, MAX_CONTRACT_ID_BYTES, PoisonMessageRecord, ScenarioJob,
    ScenarioRunResult, ScenarioRunStatus, build_terminal_event_id, now_unix_ms,
};
use crate::domain::coordination::{
    ClaimOutcome, CoordinationError, DispatchOutcome, DispatchSpec, DispatchStore, ExecutionClaim,
    ExecutionLease, ExecutionLeaseStore, LeaderElector, LeaderLease, LeadershipOutcome,
    TerminalOutcome,
};
use crate::domain::error::ContractError;
use crate::domain::scenario::{Scenario, StepPorts};
use crate::infrastructure::metrics as runtime_metrics;

const TARGET_SPS_PER_SLICE: f64 = 10.0;
const TARGET_CONCURRENCY_PER_SLICE: usize = 25;
const MAX_AUTO_SLICES: u32 = 128;

#[derive(Clone)]
pub struct ScenarioExecutionPlan {
    pub scenario: Scenario,
    pub ports: StepPorts,
    /// Deterministic runtime semantics that are not represented in the YAML
    /// step shape (engine version, deadlines, and descriptor contents).
    pub execution_semantics_fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct NodeRuntimeConfig {
    pub leader_renew_interval: Duration,
    pub scheduler_tick_interval: Duration,
    pub worker_max_retries: u32,
    pub worker_retry_base_delay: Duration,
    pub worker_retry_max_delay: Duration,
    pub worker_queue_capacity: usize,
    pub execution_renew_interval: Duration,
    pub shutdown_drain_timeout: Duration,
    /// Hard upper bound for execution plus terminal settlement. Reaching it is
    /// a fail-stop event: the source offset remains uncommitted and the node
    /// restarts before Kafka's max-poll interval can be violated silently.
    pub max_processing_interval: Duration,
    pub max_job_duration: Duration,
    pub max_scenarios_per_sec: f64,
    pub max_concurrency: usize,
    pub scenario_timeout: Option<Duration>,
    pub startup_burst: usize,
}

impl Default for NodeRuntimeConfig {
    fn default() -> Self {
        Self {
            leader_renew_interval: Duration::from_secs(3),
            scheduler_tick_interval: Duration::from_millis(500),
            worker_max_retries: 2,
            worker_retry_base_delay: Duration::from_millis(500),
            worker_retry_max_delay: Duration::from_secs(30),
            worker_queue_capacity: 64,
            execution_renew_interval: Duration::from_secs(10),
            shutdown_drain_timeout: Duration::from_secs(30),
            max_processing_interval: Duration::from_secs(299),
            max_job_duration: Duration::from_secs(60),
            max_scenarios_per_sec: 1_000.0,
            max_concurrency: 256,
            scenario_timeout: Some(Duration::from_secs(30)),
            startup_burst: 0,
        }
    }
}

#[async_trait]
pub trait JobPublisher: Send + Sync {
    /// Success means the Kafka broker acknowledged delivery according to the
    /// configured producer acknowledgement policy.
    async fn publish_job(&self, key: &str, job: &ScenarioJob) -> Result<(), String>;
}

#[async_trait]
pub trait ResultPublisher: Send + Sync {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String>;
}

#[async_trait]
pub trait DlqPublisher: Send + Sync {
    async fn publish_failed_job(&self, key: &str, job: &FailedScenarioJob) -> Result<(), String>;
    async fn publish_poison(&self, record: &PoisonMessageRecord) -> Result<(), String>;
}

#[async_trait]
pub trait JobConsumer: Send + Sync {
    type Item: CommitableJob + Send;
    async fn recv(&self) -> Result<Option<Self::Item>, String>;
}

#[async_trait]
pub trait CommitableJob: Send + Sync {
    /// Decode errors retain the source record so the poison message can reach a
    /// deterministic terminal DLQ disposition.
    fn job(&self) -> Result<&ScenarioJob, ContractError>;
    fn poison_record(&self, reason: String) -> PoisonMessageRecord;
    fn source_topic(&self) -> Option<&str> {
        None
    }
    fn source_partition(&self) -> Option<i32> {
        None
    }
    fn source_offset(&self) -> Option<i64> {
        None
    }
    /// Must wait for broker acknowledgement. Kafka commits are cumulative per
    /// partition, so a failed commit stops this worker before later records.
    async fn commit(self) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobDisposition {
    ResultPublished,
    RetryPublished,
    DeadLetterPublished,
    DuplicateCompleted,
    ExecutionLeaseBusy { retry_after: Duration },
    RetryLater,
}

#[derive(Clone, Debug)]
struct ObservedLeaderLease {
    lease: LeaderLease,
    valid_until: Instant,
}

impl ObservedLeaderLease {
    fn new(lease: LeaderLease, renew_interval: Duration, request_started: Instant) -> Self {
        // Redis TIME is authoritative for the stored lease, but comparing its
        // absolute timestamp to another host's wall clock is unsafe. Convert
        // the returned TTL into a conservative process-local monotonic budget
        // anchored before the request. Redis may have started the TTL at any
        // point during that request, so response latency consumes budget and
        // one renewal interval remains reserved.
        let valid_until = conservative_lease_deadline(request_started, lease.ttl, renew_interval);
        Self { lease, valid_until }
    }
}

fn conservative_lease_deadline(
    request_started: Instant,
    ttl: Duration,
    reserve: Duration,
) -> Instant {
    request_started
        .checked_add(ttl.saturating_sub(reserve))
        .unwrap_or(request_started)
}

fn lease_response_has_safe_budget(
    request_started: Instant,
    ttl: Duration,
    reserve: Duration,
) -> bool {
    conservative_lease_deadline(request_started, ttl, reserve) > Instant::now()
}

fn interval_remaining(request_started: Instant, interval: Duration) -> Duration {
    interval.saturating_sub(Instant::now().saturating_duration_since(request_started))
}

impl JobDisposition {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::ExecutionLeaseBusy { .. } | Self::RetryLater)
    }
}

pub struct PulseNodeDependencies<E, S, JP, JC, I, RP, DP>
where
    E: LeaderElector,
    S: DispatchStore,
    JP: JobPublisher,
    JC: JobConsumer,
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    pub elector: Arc<E>,
    pub due_store: Arc<S>,
    pub job_publisher: Arc<JP>,
    pub job_consumer: Arc<JC>,
    pub idempotency_store: Arc<I>,
    pub result_publisher: Arc<RP>,
    pub dlq_publisher: Arc<DP>,
}

pub struct PulseNode<E, S, JP, JC, I, RP, DP>
where
    E: LeaderElector,
    S: DispatchStore,
    JP: JobPublisher,
    JC: JobConsumer,
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    elector: Arc<E>,
    due_store: Arc<S>,
    job_publisher: Arc<JP>,
    job_consumer: Arc<JC>,
    idempotency_store: Arc<I>,
    result_publisher: Arc<RP>,
    dlq_publisher: Arc<DP>,
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    config: NodeRuntimeConfig,
}

impl<E, S, JP, JC, I, RP, DP> PulseNode<E, S, JP, JC, I, RP, DP>
where
    E: LeaderElector + 'static,
    S: DispatchStore + 'static,
    JP: JobPublisher + 'static,
    JC: JobConsumer + 'static,
    JC::Item: 'static,
    I: ExecutionLeaseStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    pub fn new(
        deps: PulseNodeDependencies<E, S, JP, JC, I, RP, DP>,
        plans: Vec<ScenarioExecutionPlan>,
        config: NodeRuntimeConfig,
    ) -> Self {
        let plans = plans
            .into_iter()
            .map(|plan| (plan.scenario.name.clone(), plan))
            .collect();
        Self {
            elector: deps.elector,
            due_store: deps.due_store,
            job_publisher: deps.job_publisher,
            job_consumer: deps.job_consumer,
            idempotency_store: deps.idempotency_store,
            result_publisher: deps.result_publisher,
            dlq_publisher: deps.dlq_publisher,
            plans: Arc::new(plans),
            config,
        }
    }

    pub async fn run(self, mut external_shutdown: watch::Receiver<bool>) {
        let (shutdown_tx, shutdown_rx) = watch::channel(*external_shutdown.borrow());
        let shutdown_forwarder = {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                while external_shutdown.changed().await.is_ok() {
                    if *external_shutdown.borrow() {
                        let _ = shutdown_tx.send(true);
                        return;
                    }
                }
            })
        };

        let (leader_tx, leader_rx) = watch::channel::<Option<ObservedLeaderLease>>(None);
        let (scheduler_stopped_tx, scheduler_stopped_rx) = oneshot::channel();
        let (job_tx, job_rx) = mpsc::channel(self.config.worker_queue_capacity.max(1));
        let mut tasks = JoinSet::new();

        tasks.spawn(leader_election_loop(
            self.elector.clone(),
            leader_tx,
            self.config.leader_renew_interval,
            scheduler_stopped_rx,
            shutdown_rx.clone(),
        ));
        tasks.spawn(scheduler_loop(
            self.plans.clone(),
            self.due_store.clone(),
            self.job_publisher.clone(),
            leader_rx,
            self.config.scheduler_tick_interval,
            self.config.worker_max_retries,
            self.config.startup_burst,
            scheduler_stopped_tx,
            shutdown_rx.clone(),
        ));
        tasks.spawn(consumer_pump(
            self.job_consumer.clone(),
            job_tx,
            shutdown_rx.clone(),
        ));

        let worker_runtime = WorkerRuntime {
            lease_store: self.idempotency_store,
            retry_publisher: self.job_publisher,
            result_publisher: self.result_publisher,
            dlq_publisher: self.dlq_publisher,
            config: self.config,
        };
        tasks.spawn(worker_loop(self.plans, job_rx, worker_runtime, shutdown_rx));

        while let Some(result) = tasks.join_next().await {
            if let Err(join_error) = result {
                error!(error = %join_error, "runtime loop exited unexpectedly");
            }
            if !*shutdown_tx.borrow() {
                error!("runtime component stopped; initiating coordinated shutdown");
                let _ = shutdown_tx.send(true);
            }
        }
        shutdown_forwarder.abort();
    }
}

async fn leader_election_loop<E: LeaderElector>(
    elector: Arc<E>,
    leader_tx: watch::Sender<Option<ObservedLeaderLease>>,
    renew_interval: Duration,
    scheduler_stopped: oneshot::Receiver<()>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut current: Option<LeaderLease> = None;
    loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }

        let request_started = Instant::now();
        match elector.acquire_or_renew(current.as_ref()).await {
            Ok(LeadershipOutcome::Acquired(lease)) => {
                current = Some(lease.clone());
                let observed = ObservedLeaderLease::new(lease, renew_interval, request_started);
                if observed.valid_until > Instant::now() {
                    info!(
                        owner_token = %observed.lease.owner_token,
                        fence = observed.lease.fencing_token,
                        "leadership acquired"
                    );
                    runtime_metrics::set_is_leader(true);
                    runtime_metrics::record_leadership_change("acquired");
                    let _ = leader_tx.send(Some(observed));
                } else {
                    warn!("leader acquire response exhausted its conservative local lease budget");
                    runtime_metrics::record_leadership_renewal_failure("response_budget");
                    runtime_metrics::set_is_leader(false);
                    let _ = leader_tx.send(None);
                }
            }
            Ok(LeadershipOutcome::Renewed(lease)) => {
                current = Some(lease.clone());
                let observed = ObservedLeaderLease::new(lease, renew_interval, request_started);
                if observed.valid_until > Instant::now() {
                    if leader_tx.borrow().is_none() {
                        runtime_metrics::record_leadership_change("acquired");
                    }
                    runtime_metrics::set_is_leader(true);
                    let _ = leader_tx.send(Some(observed));
                } else {
                    if leader_tx.borrow().is_some() {
                        runtime_metrics::record_leadership_change("lost");
                    }
                    warn!("leader renewal response exhausted its conservative local lease budget");
                    runtime_metrics::record_leadership_renewal_failure("response_budget");
                    runtime_metrics::set_is_leader(false);
                    let _ = leader_tx.send(None);
                }
            }
            Ok(LeadershipOutcome::Follower { retry_after }) => {
                if current.take().is_some() {
                    warn!("leadership lost to another owner");
                    runtime_metrics::record_leadership_change("lost");
                }
                runtime_metrics::set_is_leader(false);
                let _ = leader_tx.send(None);
                let wait = interval_remaining(request_started, retry_after.min(renew_interval));
                if wait_or_shutdown(wait, &mut shutdown_rx).await {
                    break;
                }
                continue;
            }
            Err(coordination_error) => {
                // Dependency failure is operationally distinct from a legitimate
                // follower outcome, but both must fence local dispatch immediately.
                error!(error = %coordination_error, "leader coordination failed");
                if current.take().is_some() {
                    runtime_metrics::record_leadership_change("lost");
                }
                runtime_metrics::record_leadership_renewal_failure(coordination_error_class(
                    &coordination_error,
                ));
                runtime_metrics::set_is_leader(false);
                let _ = leader_tx.send(None);
            }
        }

        if wait_or_shutdown(
            interval_remaining(request_started, renew_interval),
            &mut shutdown_rx,
        )
        .await
        {
            break;
        }
    }

    let _ = leader_tx.send(None);
    runtime_metrics::set_is_leader(false);
    let _ = timeout(renew_interval.saturating_mul(2), scheduler_stopped).await;
    if let Some(lease) = current {
        if let Err(error) = elector.relinquish(&lease).await {
            warn!(error = %error, "failed to relinquish leadership during shutdown");
        } else {
            info!("leadership relinquished after scheduler stopped");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn scheduler_loop<S: DispatchStore, JP: JobPublisher>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    dispatch_store: Arc<S>,
    publisher: Arc<JP>,
    mut leader_rx: watch::Receiver<Option<ObservedLeaderLease>>,
    tick_interval: Duration,
    worker_max_retries: u32,
    startup_burst: usize,
    scheduler_stopped: oneshot::Sender<()>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    'scheduler: loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }

        let Some(mut leader) = leader_rx.borrow().clone() else {
            tokio::select! {
                changed = leader_rx.changed() => {
                    if changed.is_err() { break; }
                }
                _ = shutdown_rx.changed() => {}
            }
            continue;
        };

        for (scenario_id, plan) in plans.iter() {
            if shutdown_requested(&shutdown_rx) || !lease_is_current(&leader_rx, &leader) {
                continue 'scheduler;
            }

            let slices = calculate_slices(
                plan.scenario.config.scenarios_per_sec,
                plan.scenario.config.max_concurrency,
                plan.scenario.config.duration,
            );
            let fingerprint =
                execution_plan_fingerprint(plan, slices, startup_burst, worker_max_retries);
            let spec = DispatchSpec::new(
                scenario_id,
                CURRENT_CONTRACT_VERSION,
                slices,
                plan.scenario.config.repeat.clone(),
                fingerprint,
            );
            let window = match dispatch_store.prepare_window(&spec, &leader.lease).await {
                Ok(DispatchOutcome::Ready(window)) => window,
                Ok(DispatchOutcome::NotDue { .. } | DispatchOutcome::Finished) => continue,
                Err(CoordinationError::StaleOwner { .. }) => {
                    warn!(scenario = %scenario_id, "stale leader stopped dispatch");
                    continue 'scheduler;
                }
                Err(error) => {
                    error!(scenario = %scenario_id, error = %error, "dispatch coordination failed");
                    continue;
                }
            };
            runtime_metrics::set_incomplete_dispatch_slices(
                scenario_id,
                u32::try_from(window.missing_slices.len()).unwrap_or(u32::MAX),
            );
            let lag_ms = now_unix_ms().saturating_sub(window.scheduled_at_unix_ms);
            runtime_metrics::observe_schedule_lag(
                scenario_id,
                Duration::from_millis(u64::try_from(lag_ms).unwrap_or(u64::MAX)),
            );

            if !lease_is_current(&leader_rx, &leader) {
                continue 'scheduler;
            }
            if let Err(error) = dispatch_store
                .register_run(&window, plan.scenario.config.duration)
                .await
            {
                error!(
                    scenario = %scenario_id,
                    run_id = %window.run_id,
                    error = %error,
                    "run aggregation registration failed; no slices will be published"
                );
                continue;
            }

            for slice_index in window.missing_slices.iter().copied() {
                if shutdown_requested(&shutdown_rx) || !lease_is_current(&leader_rx, &leader) {
                    warn!(scenario = %scenario_id, "dispatch interrupted by shutdown or leadership loss");
                    continue 'scheduler;
                }

                let Some(execution_key) = window.execution_key(slice_index) else {
                    error!(scenario = %scenario_id, slice_index, "dispatch window returned an invalid slice index");
                    continue 'scheduler;
                };
                let slice = JobSlice {
                    index: slice_index,
                    total: window.total_slices,
                };
                let job = ScenarioJob {
                    schema_version: window.contract_version,
                    scenario_id: scenario_id.clone(),
                    run_id: window.run_id.clone(),
                    execution_key,
                    plan_fingerprint: window.plan_fingerprint.clone(),
                    scheduled_at_unix_ms: window.scheduled_at_unix_ms,
                    not_before_unix_ms: 0,
                    slice,
                    load: slice_load(
                        &plan.scenario,
                        window.total_slices,
                        slice_index,
                        startup_burst,
                    ),
                    attempt: 0,
                    max_retries: worker_max_retries,
                };
                let key = plan.scenario.config.partition_key_strategy.key_for(&job);

                let publication = publisher.publish_job(&key, &job);
                let lease_expiry = sleep_until(leader.valid_until);
                tokio::pin!(publication);
                tokio::pin!(lease_expiry);
                let publish_result = loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_rx.changed() => {
                            continue 'scheduler;
                        }
                        _ = &mut lease_expiry => {
                            warn!(scenario = %scenario_id, slice_index, "job publication cancelled at local leadership deadline");
                            continue 'scheduler;
                        }
                        changed = leader_rx.changed() => {
                            if changed.is_err() || !leader_identity_is_current(&leader_rx, &leader) {
                                warn!(scenario = %scenario_id, slice_index, "job publication cancelled after leadership loss");
                                continue 'scheduler;
                            }
                            // A successful renewal updates the monotonic local
                            // deadline while retaining the same owner/fence.
                            leader = leader_rx
                                .borrow()
                                .clone()
                                .expect("current leader identity was verified");
                            lease_expiry.as_mut().reset(leader.valid_until);
                        }
                        result = &mut publication => break result,
                    }
                };
                if let Err(error) = publish_result {
                    runtime_metrics::record_scheduler_job_publish_failed(scenario_id);
                    error!(
                        scenario = %scenario_id,
                        run_id = %job.run_id,
                        execution_key = %job.execution_key,
                        slice_index,
                        error = %error,
                        "slice publication failed; dispatch ledger remains incomplete"
                    );
                    break;
                }
                runtime_metrics::record_scheduler_job_published(scenario_id);

                match dispatch_store
                    .ack_slice(&window, slice_index, &leader.lease)
                    .await
                {
                    Ok(progress) => {
                        let remaining = match progress {
                            crate::domain::coordination::DispatchProgress::Pending {
                                remaining_slices,
                            } => remaining_slices,
                            crate::domain::coordination::DispatchProgress::Complete => 0,
                        };
                        runtime_metrics::set_incomplete_dispatch_slices(scenario_id, remaining);
                        info!(
                            scenario = %scenario_id,
                            run_id = %job.run_id,
                            execution_key = %job.execution_key,
                            slice_index,
                            progress = ?progress,
                            "slice publication acknowledged"
                        );
                    }
                    Err(CoordinationError::StaleOwner { .. }) => {
                        // Kafka may have accepted the slice. Leaving it unacknowledged
                        // deliberately causes a deterministic duplicate, never a loss.
                        warn!(scenario = %scenario_id, slice_index, "leadership lost before dispatch acknowledgement");
                        continue 'scheduler;
                    }
                    Err(error) => {
                        error!(scenario = %scenario_id, slice_index, error = %error, "failed to persist dispatch acknowledgement");
                        break;
                    }
                }
            }
        }

        if wait_or_shutdown(tick_interval, &mut shutdown_rx).await {
            break;
        }
    }
    info!("scheduler stopped");
    let _ = scheduler_stopped.send(());
}

async fn consumer_pump<JC: JobConsumer>(
    consumer: Arc<JC>,
    sender: mpsc::Sender<JC::Item>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    info!(capacity = sender.max_capacity(), "Kafka intake started");
    loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }
        // Reserve bounded local capacity *before* fetching another Kafka
        // record. This prevents a received record from being held outside the
        // queue while waiting for space and keeps poll gaps bounded by one job
        // processing interval rather than by the entire queue backlog.
        let permit = tokio::select! {
            permit = sender.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
            _ = shutdown_rx.changed() => continue,
        };
        let received = tokio::select! {
            received = consumer.recv() => received,
            _ = shutdown_rx.changed() => continue,
        };
        match received {
            Ok(Some(record)) => {
                runtime_metrics::record_worker_job_received();
                permit.send(record);
            }
            Ok(None) => {}
            Err(error) => {
                runtime_metrics::record_worker_consume_error();
                error!(error = %error, "failed to consume Kafka job");
                if wait_or_shutdown(Duration::from_secs(1), &mut shutdown_rx).await {
                    break;
                }
            }
        }
    }
    info!("Kafka intake stopped");
}

struct WorkerRuntime<I, RP, DP>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    lease_store: Arc<I>,
    retry_publisher: Arc<dyn JobPublisher>,
    result_publisher: Arc<RP>,
    dlq_publisher: Arc<DP>,
    config: NodeRuntimeConfig,
}

async fn worker_loop<M, I, RP, DP>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    mut receiver: mpsc::Receiver<M>,
    runtime: WorkerRuntime<I, RP, DP>,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    M: CommitableJob + 'static,
    I: ExecutionLeaseStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    info!("worker processor started");
    loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }
        let record = tokio::select! {
            record = receiver.recv() => record,
            _ = shutdown_rx.changed() => {
                if shutdown_requested(&shutdown_rx) { break; }
                continue;
            }
        };
        let Some(record) = record else { break };
        if shutdown_requested(&shutdown_rx) {
            info!(
                kafka_topic = record.source_topic().unwrap_or("unknown"),
                kafka_partition = record.source_partition(),
                kafka_offset = record.source_offset(),
                "shutdown began before queued job acceptance; source remains uncommitted"
            );
            break;
        }
        let processing_started = Instant::now();
        let kafka_topic = record.source_topic().unwrap_or("unknown").to_string();
        let kafka_partition = record.source_partition();
        let kafka_offset = record.source_offset();
        let (scenario_id, run_id, execution_key, slice, attempt) = match record.job() {
            Ok(job) => (
                job.scenario_id.clone(),
                job.run_id.clone(),
                job.execution_key.clone(),
                format!("{}/{}", job.slice.index, job.slice.total),
                job.attempt.to_string(),
            ),
            Err(_) => (
                "poison".to_string(),
                "poison".to_string(),
                "poison".to_string(),
                "unknown".to_string(),
                "unknown".to_string(),
            ),
        };
        info!(
            scenario_id,
            run_id,
            execution_key,
            slice,
            attempt,
            kafka_topic,
            kafka_partition,
            kafka_offset,
            "job processing started"
        );

        let disposition = {
            let processing = timeout(
                runtime.config.max_processing_interval,
                process_until_terminal(&record, &plans, &runtime, shutdown_rx.clone()),
            );
            tokio::pin!(processing);
            tokio::select! {
                disposition = &mut processing => match disposition {
                    Ok(disposition) => disposition,
                    Err(_) => {
                        runtime_metrics::observe_job_processing(processing_started.elapsed());
                        error!(
                            max_processing_ms = runtime.config.max_processing_interval.as_millis(),
                            "job processing exceeded its Kafka-safe bound; source remains uncommitted and the worker will stop"
                        );
                        break;
                    }
                },
                _ = shutdown_rx.changed() => {
                    if shutdown_requested(&shutdown_rx) {
                        match timeout(runtime.config.shutdown_drain_timeout, &mut processing).await {
                            Ok(Ok(disposition)) => disposition,
                            Ok(Err(_)) => {
                                runtime_metrics::observe_job_processing(processing_started.elapsed());
                                error!(
                                    max_processing_ms = runtime.config.max_processing_interval.as_millis(),
                                    "job processing exceeded its Kafka-safe bound during shutdown; source remains uncommitted"
                                );
                                break;
                            }
                            Err(_) => {
                                runtime_metrics::observe_job_processing(processing_started.elapsed());
                                warn!("shutdown drain deadline expired; current source offset remains uncommitted");
                                break;
                            }
                        }
                    } else {
                        match processing.await {
                            Ok(disposition) => disposition,
                            Err(_) => {
                                runtime_metrics::observe_job_processing(processing_started.elapsed());
                                error!(
                                    max_processing_ms = runtime.config.max_processing_interval.as_millis(),
                                    "job processing exceeded its Kafka-safe bound; source remains uncommitted"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        };

        let Some(disposition) = disposition else {
            runtime_metrics::observe_job_processing(processing_started.elapsed());
            break;
        };
        debug_assert!(disposition.is_terminal());
        let commit_budget = runtime
            .config
            .max_processing_interval
            .saturating_sub(processing_started.elapsed());
        if commit_budget.is_zero() {
            runtime_metrics::record_worker_job_commit_failure();
            runtime_metrics::observe_job_processing(processing_started.elapsed());
            error!(
                ?disposition,
                "no Kafka-safe budget remains for the synchronous source commit; stopping with the source unsettled"
            );
            break;
        }
        match timeout(commit_budget, record.commit()).await {
            Ok(Ok(())) => {
                runtime_metrics::record_worker_job_commit_success();
                runtime_metrics::observe_job_processing(processing_started.elapsed());
                info!(
                    scenario_id,
                    run_id,
                    execution_key,
                    slice,
                    attempt,
                    kafka_topic,
                    kafka_partition,
                    kafka_offset,
                    ?disposition,
                    "source offset committed after durable terminal disposition"
                );
            }
            Ok(Err(error)) => {
                runtime_metrics::record_worker_job_commit_failure();
                runtime_metrics::observe_job_processing(processing_started.elapsed());
                error!(error = %error, ?disposition, "synchronous source offset commit failed; stopping worker to preserve partition ordering");
                break;
            }
            Err(_) => {
                runtime_metrics::record_worker_job_commit_failure();
                runtime_metrics::observe_job_processing(processing_started.elapsed());
                error!(
                    commit_budget_ms = commit_budget.as_millis(),
                    ?disposition,
                    "synchronous source offset commit exceeded the Kafka-safe budget; stopping with the source unsettled"
                );
                break;
            }
        }

        if shutdown_requested(&shutdown_rx) {
            break;
        }
    }
    info!("worker processor stopped");
}

async fn process_until_terminal<M, I, RP, DP>(
    record: &M,
    plans: &Arc<HashMap<String, ScenarioExecutionPlan>>,
    runtime: &WorkerRuntime<I, RP, DP>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Option<JobDisposition>
where
    M: CommitableJob,
    I: ExecutionLeaseStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    if let Ok(job) = record.job() {
        let delay = deferred_job_delay(job, now_unix_ms());
        if !delay.is_zero() && delay <= runtime.config.worker_retry_max_delay {
            info!(
                execution_key = %job.execution_key,
                attempt = job.attempt,
                delay_ms = delay.as_millis(),
                "waiting for durable retry not-before time before claiming execution"
            );
            runtime_metrics::set_retry_queue_depth(1);
            if shutdown_requested(&shutdown_rx) || wait_or_shutdown(delay, &mut shutdown_rx).await {
                runtime_metrics::set_retry_queue_depth(0);
                return None;
            }
            runtime_metrics::set_retry_queue_depth(0);
        }
    }

    let mut record_job_age = true;
    loop {
        let disposition = process_once(record, plans, runtime, record_job_age).await;
        record_job_age = false;
        match disposition {
            disposition if disposition.is_terminal() => return Some(disposition),
            JobDisposition::ExecutionLeaseBusy { retry_after } => {
                // A rebalance can redeliver an uncommitted record while its
                // previous owner is still executing it. This is not a
                // dependency error and must not crash the second worker. Keep
                // the source record unsettled and re-check until the durable
                // execution record becomes terminal or the outer Kafka-safe
                // processing deadline expires. The independent, bounded
                // consumer pump continues polling while this worker waits.
                let delay = if retry_after.is_zero() {
                    runtime.config.worker_retry_base_delay
                } else {
                    retry_after.min(runtime.config.worker_retry_max_delay)
                };
                let execution_key = record
                    .job()
                    .map(|job| job.execution_key.as_str())
                    .unwrap_or("poison");
                info!(
                    execution_key,
                    retry_after_ms = retry_after.as_millis(),
                    recheck_in_ms = delay.as_millis(),
                    "retaining uncommitted source while another lease owner executes the job"
                );
                if shutdown_requested(&shutdown_rx)
                    || wait_or_shutdown(delay, &mut shutdown_rx).await
                {
                    return None;
                }
            }
            JobDisposition::RetryLater => {
                // Every publication/coordination helper has already consumed
                // its bounded local retry budget. Re-entering process_once
                // could execute a target slice again in the same process
                // without a durable retry disposition. Fail-stop instead:
                // Kafka retains the source offset and the record is recovered
                // after restart/rebalance.
                error!(
                    "job exhausted bounded local settlement retries; stopping worker with source offset unsettled"
                );
                return None;
            }
            _ => unreachable!("terminal dispositions returned above"),
        }
    }
}

async fn process_once<M, I, RP, DP>(
    record: &M,
    plans: &Arc<HashMap<String, ScenarioExecutionPlan>>,
    runtime: &WorkerRuntime<I, RP, DP>,
    record_job_age: bool,
) -> JobDisposition
where
    M: CommitableJob,
    I: ExecutionLeaseStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    let job = match record.job() {
        Ok(job) => job,
        Err(contract_error) => {
            let poison = record.poison_record(contract_error.to_string());
            return match publish_poison_with_retry(&runtime.dlq_publisher, &poison, runtime).await {
                Ok(()) => JobDisposition::DeadLetterPublished,
                Err(error) => {
                    error!(event_id = %poison.event_id, error = %error, "poison DLQ publication failed; source remains uncommitted");
                    JobDisposition::RetryLater
                }
            };
        }
    };
    if record_job_age {
        let job_age_ms = now_unix_ms().saturating_sub(job.scheduled_at_unix_ms);
        let job_age = Duration::from_millis(u64::try_from(job_age_ms).unwrap_or(u64::MAX));
        runtime_metrics::observe_job_age(job_age);
        if job.attempt > 0 {
            runtime_metrics::observe_retry_job_age(job_age);
        }
    }

    if let Err(contract_error) = job.validate_limits(
        runtime.config.max_job_duration,
        runtime.config.max_scenarios_per_sec,
        runtime.config.max_concurrency,
    ) {
        let failed = failed_job(job, format!("invalid job contract: {contract_error}"));
        return match publish_failed_with_retry(
            &runtime.dlq_publisher,
            &failed,
            scenario_metric_label(plans, &job.scenario_id),
            runtime,
        )
        .await
        {
            Ok(()) => JobDisposition::DeadLetterPublished,
            Err(error) => {
                error!(execution_key = %job.execution_key, error = %error, "invalid-job DLQ publication failed; source remains uncommitted");
                JobDisposition::RetryLater
            }
        };
    }

    if let Some(reason) = deterministic_job_plan_error(job, plans, runtime) {
        let failed = failed_job(job, reason);
        return match publish_failed_with_retry(
            &runtime.dlq_publisher,
            &failed,
            scenario_metric_label(plans, &job.scenario_id),
            runtime,
        )
        .await
        {
            Ok(()) => JobDisposition::DeadLetterPublished,
            Err(error) => {
                error!(execution_key = %job.execution_key, error = %error, "job-plan mismatch DLQ publication failed; source remains uncommitted");
                JobDisposition::RetryLater
            }
        };
    }

    let retry_delay = deferred_job_delay(job, now_unix_ms());
    if retry_delay > runtime.config.worker_retry_max_delay {
        let failed = failed_job(
            job,
            format!(
                "invalid retry deferral: remaining not-before delay {} ms exceeds configured maximum {} ms",
                retry_delay.as_millis(),
                runtime.config.worker_retry_max_delay.as_millis()
            ),
        );
        return match publish_failed_with_retry(
            &runtime.dlq_publisher,
            &failed,
            scenario_metric_label(plans, &job.scenario_id),
            runtime,
        )
        .await
        {
            Ok(()) => JobDisposition::DeadLetterPublished,
            Err(error) => {
                error!(execution_key = %job.execution_key, error = %error, "invalid retry-deferral DLQ publication failed; source remains uncommitted");
                JobDisposition::RetryLater
            }
        };
    }
    if job.load.startup_burst == 0
        && job.load.scenarios_per_sec * job.load.duration.as_secs_f64() < 1.0
    {
        let failed = failed_job(
            job,
            "invalid load plan: no paced scenario can start inside this window without an explicit startup burst"
                .to_string(),
        );
        return match publish_failed_with_retry(
            &runtime.dlq_publisher,
            &failed,
            scenario_metric_label(plans, &job.scenario_id),
            runtime,
        )
        .await
        {
            Ok(()) => JobDisposition::DeadLetterPublished,
            Err(error) => {
                error!(execution_key = %job.execution_key, error = %error, "zero-start load-plan DLQ publication failed; source remains uncommitted");
                JobDisposition::RetryLater
            }
        };
    }

    let claim = ExecutionClaim::new(&job.execution_key, job.attempt);
    let claim_started = Instant::now();
    let lease = match runtime.lease_store.claim(&claim).await {
        Ok(ClaimOutcome::Acquired(lease)) => lease,
        Ok(ClaimOutcome::AlreadyCompleted(completed)) => {
            runtime_metrics::record_execution_lease("completed_duplicate");
            runtime_metrics::record_worker_duplicate_job();
            info!(
                execution_key = %job.execution_key,
                attempt = job.attempt,
                terminal_outcome = ?completed.outcome,
                "verified durable duplicate"
            );
            return JobDisposition::DuplicateCompleted;
        }
        Ok(ClaimOutcome::Busy { retry_after }) => {
            runtime_metrics::record_execution_lease("busy");
            info!(execution_key = %job.execution_key, retry_after_ms = retry_after.as_millis(), "execution lease is busy");
            return JobDisposition::ExecutionLeaseBusy { retry_after };
        }
        Err(error) => {
            runtime_metrics::record_execution_lease("error");
            error!(execution_key = %job.execution_key, error = %error, "execution claim failed; not treating dependency error as duplicate");
            return JobDisposition::RetryLater;
        }
    };

    let lease_valid_until = conservative_lease_deadline(
        claim_started,
        lease.ttl,
        runtime.config.execution_renew_interval,
    );
    if lease_valid_until <= Instant::now() {
        runtime_metrics::record_execution_lease("error");
        runtime_metrics::record_execution_lease_renewal_failure("response_budget");
        warn!(
            execution_key = %job.execution_key,
            owner_token = %lease.owner_token,
            "execution claim response exhausted its conservative local lease budget; target will not run"
        );
        return JobDisposition::RetryLater;
    }

    runtime_metrics::record_execution_lease(if lease.recovered {
        "recovered"
    } else {
        "acquired"
    });
    info!(
        scenario_id = %job.scenario_id,
        run_id = %job.run_id,
        execution_key = %job.execution_key,
        slice = job.slice.index,
        attempt = job.attempt,
        lease_owner = %lease.owner_token,
        lease_recovered = lease.recovered,
        "execution lease acquired"
    );
    let first_renewal_at = claim_started
        .checked_add(runtime.config.execution_renew_interval)
        .unwrap_or(claim_started);
    execute_with_lease(
        job,
        plans.get(&job.scenario_id),
        lease,
        first_renewal_at,
        lease_valid_until,
        runtime,
    )
    .await
}

async fn execute_with_lease<I, RP, DP>(
    job: &ScenarioJob,
    plan: Option<&ScenarioExecutionPlan>,
    lease: ExecutionLease,
    first_renewal_at: Instant,
    lease_valid_until: Instant,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> JobDisposition
where
    I: ExecutionLeaseStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    let renewal = lease_renewal_until_loss(
        runtime.lease_store.clone(),
        lease.clone(),
        runtime.config.execution_renew_interval,
        first_renewal_at,
        lease_valid_until,
    );

    let settlement = execute_and_publish(job, plan, &lease, runtime);
    tokio::pin!(settlement);
    tokio::pin!(renewal);
    let disposition = tokio::select! {
        disposition = &mut settlement => disposition,
        error = &mut renewal => {
            error!(
                execution_key = %job.execution_key,
                attempt = job.attempt,
                owner_token = %lease.owner_token,
                error = %error,
                "execution lease lost; cancelling scenario and leaving source unsettled"
            );
            // Once ownership becomes uncertain, fail closed. Publishing a
            // retry or DLQ would itself be a follow-on decision made without
            // a current Redis fence and could add unintended target traffic.
            JobDisposition::RetryLater
        }
    };

    if matches!(disposition, JobDisposition::RetryLater)
        && let Err(error) = runtime.lease_store.release(&lease).await
    {
        warn!(execution_key = %job.execution_key, error = %error, "could not release unsettled execution lease; expiry will enable recovery");
    }
    disposition
}

/// Renews until ownership is lost or Redis fails. This future is deliberately
/// not spawned: dropping a timed-out/cancelled job also drops its renewer, so a
/// cancelled execution cannot keep a lease alive in the background.
async fn lease_renewal_until_loss<I: ExecutionLeaseStore>(
    store: Arc<I>,
    mut lease: ExecutionLease,
    interval: Duration,
    mut next_renewal_at: Instant,
    mut lease_valid_until: Instant,
) -> CoordinationError {
    loop {
        tokio::select! {
            biased;
            _ = sleep_until(lease_valid_until) => {
                let error = CoordinationError::Timeout {
                    operation: "execution_lease_local_deadline",
                };
                runtime_metrics::record_execution_lease_renewal_failure(
                    coordination_error_class(&error),
                );
                return error;
            }
            _ = sleep_until(next_renewal_at) => {}
        }
        let request_started = Instant::now();
        let renewal_result = {
            let renewal = store.renew(&lease);
            tokio::pin!(renewal);
            tokio::select! {
                biased;
                _ = sleep_until(lease_valid_until) => {
                    let error = CoordinationError::Timeout {
                        operation: "execution_lease_local_deadline",
                    };
                    runtime_metrics::record_execution_lease_renewal_failure(
                        coordination_error_class(&error),
                    );
                    return error;
                }
                result = &mut renewal => result,
            }
        };
        match renewal_result {
            Ok(renewed) => {
                let renewed_valid_until =
                    conservative_lease_deadline(request_started, renewed.ttl, interval);
                if renewed_valid_until <= Instant::now() {
                    let error = CoordinationError::Timeout {
                        operation: "execution_renew_response_budget",
                    };
                    runtime_metrics::record_execution_lease_renewal_failure(
                        coordination_error_class(&error),
                    );
                    return error;
                }
                lease = renewed;
                lease_valid_until = renewed_valid_until;
                next_renewal_at = request_started
                    .checked_add(interval)
                    .unwrap_or(request_started);
            }
            Err(error) => {
                runtime_metrics::record_execution_lease_renewal_failure(coordination_error_class(
                    &error,
                ));
                return error;
            }
        }
    }
}

async fn execute_and_publish<I, RP, DP>(
    job: &ScenarioJob,
    plan: Option<&ScenarioExecutionPlan>,
    lease: &ExecutionLease,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> JobDisposition
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let Some(plan) = plan else {
        runtime_metrics::record_worker_unknown_scenario();
        let failed = failed_job(job, "unknown scenario".to_string());
        if let Err(error) = publish_leased_failed_with_retry(
            &runtime.dlq_publisher,
            &failed,
            "unknown",
            lease,
            runtime,
        )
        .await
        {
            error!(execution_key = %job.execution_key, error = %error, "unknown-scenario DLQ publication failed");
            return JobDisposition::RetryLater;
        }
        return match complete_until_acknowledged(
            &runtime.lease_store,
            lease,
            TerminalOutcome::DeadLetterPublished,
            runtime,
        )
        .await
        {
            Ok(_) => {
                runtime_metrics::record_worker_dlq_published("unknown");
                JobDisposition::DeadLetterPublished
            }
            Err(error) => {
                error!(execution_key = %job.execution_key, error = %error, "DLQ was acknowledged but terminal lease recording failed");
                JobDisposition::RetryLater
            }
        };
    };

    let started_at = now_unix_ms();
    let report = PulseRunner::run_once(
        plan.scenario.clone(),
        plan.ports.clone(),
        RunnerConfig {
            duration: job.load.duration,
            scenarios_per_sec: job.load.scenarios_per_sec,
            max_concurrency: job.load.max_concurrency,
            scenario_timeout: runtime.config.scenario_timeout,
            startup_burst: job.load.startup_burst,
        },
    )
    .await;
    let finished_at = now_unix_ms();
    let invariant_violations = report
        .summary
        .error_counts
        .get("invariant_violation")
        .copied()
        .unwrap_or(0);
    if invariant_violations > 0 {
        error!(
            execution_key = %job.execution_key,
            invariant_violations,
            "internal invariant violation is fail-stop; source remains unsettled"
        );
        return JobDisposition::RetryLater;
    }

    let infrastructure_failures = report
        .summary
        .error_counts
        .get("pulse_infrastructure")
        .copied()
        .unwrap_or(0);
    if infrastructure_failures > 0 {
        return settle_infrastructure_failure(
            job,
            Some(plan),
            lease,
            format!(
                "scenario attempt measured {infrastructure_failures} transient Pulse infrastructure failure(s)"
            ),
            runtime,
        )
        .await;
    }

    let permanent_failures = [
        "permanent_processing",
        "invalid_scenario",
        "missing_context_var",
    ]
    .into_iter()
    .map(|kind| report.summary.error_counts.get(kind).copied().unwrap_or(0))
    .sum::<u64>();
    if permanent_failures > 0 {
        return publish_and_complete_dlq(
            job,
            lease,
            format!(
                "scenario attempt measured {permanent_failures} permanent Pulse processing failure(s)"
            ),
            &job.scenario_id,
            runtime,
        )
        .await;
    }

    let scenario_metrics = report.summary.scenario_metrics.get(&job.scenario_id);
    let success = scenario_metrics.map(|metrics| metrics.success).unwrap_or(0);
    let failure = scenario_metrics.map(|metrics| metrics.failure).unwrap_or(0);
    let status = if report.started == report.finished && failure == 0 && report.finished > 0 {
        ScenarioRunStatus::Success
    } else {
        ScenarioRunStatus::Failed
    };
    let mut error_breakdown: Vec<_> = report
        .summary
        .error_counts
        .iter()
        .map(|(kind, count)| ErrorCount {
            kind: kind.clone(),
            count: *count,
        })
        .collect();
    error_breakdown.sort_by(|left, right| left.kind.cmp(&right.kind));
    let result = ScenarioRunResult {
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: job.scenario_id.clone(),
        run_id: job.run_id.clone(),
        execution_key: job.execution_key.clone(),
        event_id: build_terminal_event_id(&job.execution_key, job.attempt, "result"),
        attempt: job.attempt,
        slice: job.slice.clone(),
        started_at_unix_ms: started_at,
        finished_at_unix_ms: finished_at,
        status,
        total: report.finished,
        success,
        failure,
        scenario_latency_p50_ms: scenario_metrics
            .map(|metrics| metrics.latency_ms.value_at_quantile(0.50))
            .unwrap_or(0),
        scenario_latency_p95_ms: scenario_metrics
            .map(|metrics| metrics.latency_ms.value_at_quantile(0.95))
            .unwrap_or(0),
        scenario_latency_p99_ms: scenario_metrics
            .map(|metrics| metrics.latency_ms.value_at_quantile(0.99))
            .unwrap_or(0),
        latency_histogram: scenario_metrics
            .map(|metrics| metrics.mergeable_latency_buckets())
            .unwrap_or_else(|| MetricsBucket::new().mergeable_latency_buckets()),
        error_breakdown,
    };

    if let Err(error) =
        publish_result_with_retry(&runtime.result_publisher, &result, lease, runtime).await
    {
        // The target slice has already executed. Publishing a fresh job attempt
        // here would turn a result-topic outage into additional target traffic
        // and would authorize the source commit even though the selected result
        // disposition never became durable. Fail-stop with the source unsettled;
        // redelivery remains at-least-once and reuses the deterministic result
        // identity if recovery has to execute the slice again.
        error!(execution_key = %job.execution_key, error = %error, "result publication exhausted its bounded local budget; source remains uncommitted and no whole-slice retry will be published");
        return JobDisposition::RetryLater;
    }
    runtime_metrics::record_worker_result_published(
        &result.scenario_id,
        status_label(&result.status),
    );

    match complete_until_acknowledged(
        &runtime.lease_store,
        lease,
        TerminalOutcome::ResultPublished,
        runtime,
    )
    .await
    {
        Ok(_) => {
            info!(
                scenario = %job.scenario_id,
                run_id = %job.run_id,
                execution_key = %job.execution_key,
                attempt = job.attempt,
                status = ?result.status,
                started = report.started,
                finished = report.finished,
                peak_pending_tasks = report.peak_pending_tasks,
                "scenario slice reached durable terminal result"
            );
            JobDisposition::ResultPublished
        }
        Err(error) => {
            // The result may already exist. Redelivery uses the same event and
            // execution identity; aggregation must ignore the duplicate.
            error!(execution_key = %job.execution_key, error = %error, "result was acknowledged but terminal lease recording failed");
            JobDisposition::RetryLater
        }
    }
}

/// Converts a classified transient Pulse failure into a durable next attempt.
/// Kafka must acknowledge the retry record before this attempt is recorded as
/// terminal; only then may the caller commit the source offset. Target-service
/// failures never enter this path.
async fn settle_infrastructure_failure<I, RP, DP>(
    job: &ScenarioJob,
    plan: Option<&ScenarioExecutionPlan>,
    lease: &ExecutionLease,
    reason: String,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> JobDisposition
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    if job.attempt >= job.max_retries {
        let metric_scenario = plan.map(|_| job.scenario_id.as_str()).unwrap_or("unknown");
        return publish_and_complete_dlq(
            job,
            lease,
            format!(
                "automatic retry attempts exhausted at attempt {}/{}: {reason}",
                job.attempt, job.max_retries
            ),
            metric_scenario,
            runtime,
        )
        .await;
    }

    let delay = retry_delay(
        runtime.config.worker_retry_base_delay,
        runtime.config.worker_retry_max_delay,
        job.attempt,
        &job.execution_key,
    );
    let mut retry_job = job.clone();
    retry_job.attempt = retry_job.attempt.saturating_add(1);
    retry_job.not_before_unix_ms = now_unix_ms().saturating_add(delay.as_millis());
    let partition_key = plan
        .map(|plan| {
            plan.scenario
                .config
                .partition_key_strategy
                .key_for(&retry_job)
        })
        .unwrap_or_else(|| retry_job.execution_key.clone());
    let metric_scenario = plan.map(|_| job.scenario_id.as_str()).unwrap_or("unknown");

    if let Err(error) = publish_retry_with_retry(
        &runtime.retry_publisher,
        &partition_key,
        &retry_job,
        metric_scenario,
        lease,
        runtime,
    )
    .await
    {
        // The bounded local publication budget is exhausted. The current
        // source remains unsettled and the worker fail-stops below.
        error!(
            execution_key = %job.execution_key,
            attempt = job.attempt,
            error = %error,
            "retry publication failed; source remains uncommitted"
        );
        return JobDisposition::RetryLater;
    }
    runtime_metrics::record_worker_retry_job_published(metric_scenario);

    match complete_until_acknowledged(
        &runtime.lease_store,
        lease,
        TerminalOutcome::RetryPublished,
        runtime,
    )
    .await
    {
        Ok(()) => {
            info!(
                scenario_id = %job.scenario_id,
                run_id = %job.run_id,
                execution_key = %job.execution_key,
                attempt = job.attempt,
                next_attempt = retry_job.attempt,
                not_before_unix_ms = retry_job.not_before_unix_ms,
                reason,
                "infrastructure failure reached durable retry disposition"
            );
            JobDisposition::RetryPublished
        }
        Err(error) => {
            error!(
                execution_key = %job.execution_key,
                attempt = job.attempt,
                error = %error,
                "retry was acknowledged but terminal lease recording failed"
            );
            JobDisposition::RetryLater
        }
    }
}

async fn publish_and_complete_dlq<I, RP, DP>(
    job: &ScenarioJob,
    lease: &ExecutionLease,
    reason: String,
    metric_scenario: &str,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> JobDisposition
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let failed = failed_job(job, reason);
    if let Err(error) = publish_leased_failed_with_retry(
        &runtime.dlq_publisher,
        &failed,
        metric_scenario,
        lease,
        runtime,
    )
    .await
    {
        error!(execution_key = %job.execution_key, error = %error, "DLQ publication failed; source remains uncommitted");
        return JobDisposition::RetryLater;
    }

    match complete_until_acknowledged(
        &runtime.lease_store,
        lease,
        TerminalOutcome::DeadLetterPublished,
        runtime,
    )
    .await
    {
        Ok(()) => {
            runtime_metrics::record_worker_dlq_published(metric_scenario);
            JobDisposition::DeadLetterPublished
        }
        Err(error) => {
            error!(execution_key = %job.execution_key, error = %error, "DLQ was acknowledged but terminal lease recording failed");
            JobDisposition::RetryLater
        }
    }
}

async fn publish_retry_with_retry<I, RP, DP>(
    publisher: &Arc<dyn JobPublisher>,
    partition_key: &str,
    retry_job: &ScenarioJob,
    metric_scenario: &str,
    lease: &ExecutionLease,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let limit = local_settlement_attempt_limit(runtime);
    for publication_attempt in 0..limit {
        verify_publication_lease(
            &runtime.lease_store,
            lease,
            runtime.config.execution_renew_interval,
        )
        .await?;
        match publisher.publish_job(partition_key, retry_job).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                runtime_metrics::record_worker_retry_job_publish_failure(metric_scenario);
                if publication_attempt + 1 >= limit {
                    return Err(error);
                }
                warn!(
                    execution_key = %retry_job.execution_key,
                    attempt = retry_job.attempt,
                    publication_attempt,
                    error = %error,
                    "retry job publication not acknowledged; retaining retry intent and source offset"
                );
            }
        }
        sleep(retry_delay(
            runtime.config.worker_retry_base_delay,
            runtime.config.worker_retry_max_delay,
            publication_attempt,
            &retry_job.execution_key,
        ))
        .await;
    }
    unreachable!("bounded retry loop always publishes or returns its last error")
}

async fn publish_result_with_retry<I, RP, DP>(
    publisher: &Arc<RP>,
    result: &ScenarioRunResult,
    lease: &ExecutionLease,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let limit = local_settlement_attempt_limit(runtime);
    for attempt in 0..limit {
        verify_publication_lease(
            &runtime.lease_store,
            lease,
            runtime.config.execution_renew_interval,
        )
        .await?;
        match publisher.publish_result(result).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                runtime_metrics::record_worker_result_publish_failure();
                if attempt + 1 >= limit {
                    return Err(error);
                }
                warn!(
                    execution_key = %result.execution_key,
                    attempt,
                    error = %error,
                    "result publication not acknowledged; retaining the execution result and retrying"
                );
            }
        }
        sleep(retry_delay(
            runtime.config.worker_retry_base_delay,
            runtime.config.worker_retry_max_delay,
            attempt,
            &result.execution_key,
        ))
        .await;
    }
    unreachable!("bounded retry loop always publishes or returns its last error")
}

async fn publish_failed_with_retry<I, RP, DP>(
    publisher: &Arc<DP>,
    failed: &FailedScenarioJob,
    metric_scenario: &str,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    publish_failed_with_optional_lease(publisher, failed, metric_scenario, None, runtime).await
}

async fn publish_leased_failed_with_retry<I, RP, DP>(
    publisher: &Arc<DP>,
    failed: &FailedScenarioJob,
    metric_scenario: &str,
    lease: &ExecutionLease,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    publish_failed_with_optional_lease(publisher, failed, metric_scenario, Some(lease), runtime)
        .await
}

async fn publish_failed_with_optional_lease<I, RP, DP>(
    publisher: &Arc<DP>,
    failed: &FailedScenarioJob,
    metric_scenario: &str,
    lease: Option<&ExecutionLease>,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let limit = local_settlement_attempt_limit(runtime);
    for attempt in 0..limit {
        if let Some(lease) = lease {
            verify_publication_lease(
                &runtime.lease_store,
                lease,
                runtime.config.execution_renew_interval,
            )
            .await?;
        }
        match publisher
            .publish_failed_job(&failed.execution_key, failed)
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                runtime_metrics::record_worker_dlq_publish_failure(metric_scenario);
                if attempt + 1 >= limit {
                    return Err(error);
                }
                warn!(
                    execution_key = %failed.execution_key,
                    attempt,
                    error = %error,
                    "DLQ publication not acknowledged; retaining the source record and retrying"
                );
            }
        }
        sleep(retry_delay(
            runtime.config.worker_retry_base_delay,
            runtime.config.worker_retry_max_delay,
            attempt,
            &failed.execution_key,
        ))
        .await;
    }
    unreachable!("bounded retry loop always publishes or returns its last error")
}

async fn verify_publication_lease<I: ExecutionLeaseStore>(
    store: &Arc<I>,
    lease: &ExecutionLease,
    renew_interval: Duration,
) -> Result<(), String> {
    let request_started = Instant::now();
    let renewed = store.renew(lease).await.map_err(|error| {
        runtime_metrics::record_execution_lease_renewal_failure(coordination_error_class(&error));
        format!(
            "execution lease could not be owner-checked immediately before publication: {error}"
        )
    })?;
    if !lease_response_has_safe_budget(request_started, renewed.ttl, renew_interval) {
        runtime_metrics::record_execution_lease_renewal_failure("response_budget");
        return Err(
            "execution lease owner check exhausted its conservative local lease budget".to_string(),
        );
    }
    Ok(())
}

async fn publish_poison_with_retry<I, RP, DP>(
    publisher: &Arc<DP>,
    poison: &PoisonMessageRecord,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let limit = local_settlement_attempt_limit(runtime);
    for attempt in 0..limit {
        match publisher.publish_poison(poison).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                runtime_metrics::record_worker_dlq_publish_failure("poison");
                if attempt + 1 >= limit {
                    return Err(error);
                }
                warn!(
                    event_id = %poison.event_id,
                    attempt,
                    error = %error,
                    "poison DLQ publication not acknowledged; retaining the source record and retrying"
                );
            }
        }
        sleep(retry_delay(
            runtime.config.worker_retry_base_delay,
            runtime.config.worker_retry_max_delay,
            attempt,
            &poison.event_id,
        ))
        .await;
    }
    unreachable!("bounded retry loop always publishes or returns its last error")
}

async fn complete_until_acknowledged<I, RP, DP>(
    store: &Arc<I>,
    lease: &ExecutionLease,
    outcome: TerminalOutcome,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Result<(), CoordinationError>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    let limit = local_settlement_attempt_limit(runtime);
    for attempt in 0..limit {
        match store.complete(lease, outcome).await {
            Ok(_) => return Ok(()),
            Err(error @ CoordinationError::StaleOwner { .. }) => return Err(error),
            Err(error) => {
                if attempt + 1 >= limit {
                    return Err(error);
                }
                warn!(
                    execution_key = %lease.execution_key,
                    owner_token = %lease.owner_token,
                    attempt,
                    error = %error,
                    "terminal publication is acknowledged but Redis completion is not; retrying owner-checked completion"
                );
            }
        }
        sleep(retry_delay(
            runtime.config.worker_retry_base_delay,
            runtime.config.worker_retry_max_delay,
            attempt,
            &lease.execution_key,
        ))
        .await;
    }
    unreachable!("bounded retry loop always completes or returns its last error")
}

fn local_settlement_attempt_limit<I, RP, DP>(runtime: &WorkerRuntime<I, RP, DP>) -> u32
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    runtime.config.worker_max_retries.saturating_add(1).max(1)
}

fn failed_job(job: &ScenarioJob, reason: String) -> FailedScenarioJob {
    FailedScenarioJob {
        schema_version: CURRENT_CONTRACT_VERSION,
        event_id: build_terminal_event_id(&job.execution_key, job.attempt, "dlq"),
        scenario_id: job.scenario_id.clone(),
        run_id: job.run_id.clone(),
        execution_key: job.execution_key.clone(),
        slice: job.slice.clone(),
        failed_at_unix_ms: now_unix_ms(),
        attempt: job.attempt,
        max_retries: job.max_retries,
        reason: bounded_failure_reason(reason),
    }
}

fn bounded_failure_reason(mut reason: String) -> String {
    const MARKER: &str = "...[truncated]";
    if reason.len() <= MAX_CONTRACT_ID_BYTES {
        return reason;
    }
    let mut boundary = MAX_CONTRACT_ID_BYTES.saturating_sub(MARKER.len());
    while boundary > 0 && !reason.is_char_boundary(boundary) {
        boundary -= 1;
    }
    reason.truncate(boundary);
    reason.push_str(MARKER);
    reason
}

fn deterministic_job_plan_error<I, RP, DP>(
    job: &ScenarioJob,
    plans: &HashMap<String, ScenarioExecutionPlan>,
    runtime: &WorkerRuntime<I, RP, DP>,
) -> Option<String>
where
    I: ExecutionLeaseStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    if job.max_retries > runtime.config.worker_max_retries {
        return Some(format!(
            "job max_retries {} exceeds configured worker ceiling {}",
            job.max_retries, runtime.config.worker_max_retries
        ));
    }
    if job.schema_version < 2 {
        return None;
    }
    let plan = plans.get(&job.scenario_id)?;
    let expected_load = slice_load(
        &plan.scenario,
        job.slice.total,
        job.slice.index,
        runtime.config.startup_burst,
    );
    if job.load.scenarios_per_sec.to_bits() != expected_load.scenarios_per_sec.to_bits()
        || job.load.duration != expected_load.duration
        || job.load.max_concurrency != expected_load.max_concurrency
        || job.load.startup_burst != expected_load.startup_burst
    {
        return Some(format!(
            "job load does not match deterministic local slice plan: expected rate={} duration_ms={} concurrency={} startup_burst={}, found rate={} duration_ms={} concurrency={} startup_burst={}",
            expected_load.scenarios_per_sec,
            expected_load.duration.as_millis(),
            expected_load.max_concurrency,
            expected_load.startup_burst,
            job.load.scenarios_per_sec,
            job.load.duration.as_millis(),
            job.load.max_concurrency,
            job.load.startup_burst,
        ));
    }
    let expected_fingerprint = execution_plan_fingerprint(
        plan,
        job.slice.total,
        runtime.config.startup_burst,
        runtime.config.worker_max_retries,
    );
    if job.plan_fingerprint != expected_fingerprint {
        return Some(format!(
            "job plan fingerprint '{}' does not match local plan '{}'",
            job.plan_fingerprint, expected_fingerprint
        ));
    }
    None
}

fn calculate_slices(scenarios_per_sec: f64, max_concurrency: usize, duration: Duration) -> u32 {
    if !scenarios_per_sec.is_finite() || scenarios_per_sec <= 0.0 || max_concurrency == 0 {
        return 1;
    }
    let slices_by_rate = (scenarios_per_sec / TARGET_SPS_PER_SLICE).ceil().max(1.0) as u32;
    let slices_by_concurrency =
        ((max_concurrency as f64) / (TARGET_CONCURRENCY_PER_SLICE as f64)).ceil() as u32;
    let concurrency_cap = u32::try_from(max_concurrency).unwrap_or(u32::MAX);
    // Never create more slices than the deterministic window can give at
    // least one paced arrival. Otherwise a valid fractional global rate can
    // turn into zero traffic in every independently paced slice.
    let expected_start_cap = (scenarios_per_sec * duration.as_secs_f64())
        .floor()
        .max(1.0)
        .min(f64::from(u32::MAX)) as u32;
    slices_by_rate
        .max(slices_by_concurrency)
        .max(1)
        .min(concurrency_cap)
        .min(expected_start_cap)
        .min(MAX_AUTO_SLICES)
}

/// Returns the exact deterministic per-slice load plan used by the scheduler.
///
/// This is intentionally exposed for dry-run validation and operational tooling
/// so those surfaces cannot drift from the runtime's rate and concurrency math.
pub fn planned_slice_loads(scenario: &Scenario, global_startup_burst: usize) -> Vec<JobLoadConfig> {
    let total = calculate_slices(
        scenario.config.scenarios_per_sec,
        scenario.config.max_concurrency,
        scenario.config.duration,
    );
    (0..total)
        .map(|index| slice_load(scenario, total, index, global_startup_burst))
        .collect()
}

fn slice_load(
    scenario: &Scenario,
    total_slices: u32,
    slice_index: u32,
    global_startup_burst: usize,
) -> JobLoadConfig {
    let total = total_slices.max(1) as usize;
    let base = scenario.config.max_concurrency / total;
    let remainder = scenario.config.max_concurrency % total;
    JobLoadConfig {
        scenarios_per_sec: scenario.config.scenarios_per_sec / total_slices.max(1) as f64,
        duration: scenario.config.duration,
        max_concurrency: base + usize::from((slice_index as usize) < remainder),
        startup_burst: slice_startup_burst(global_startup_burst, total_slices, slice_index),
    }
}

pub fn slice_startup_burst(global_burst: usize, total_slices: u32, slice_index: u32) -> usize {
    let total = total_slices.max(1) as usize;
    let base = global_burst / total;
    let remainder = global_burst % total;
    base + usize::from((slice_index as usize) < remainder)
}

pub fn execution_plan_fingerprint(
    plan: &ScenarioExecutionPlan,
    slices: u32,
    global_startup_burst: usize,
    worker_max_retries: u32,
) -> String {
    scenario_plan_fingerprint(
        &plan.scenario,
        slices,
        global_startup_burst,
        worker_max_retries,
        &plan.execution_semantics_fingerprint,
    )
}

pub fn scenario_plan_fingerprint(
    scenario: &Scenario,
    slices: u32,
    global_startup_burst: usize,
    worker_max_retries: u32,
    execution_semantics_fingerprint: &str,
) -> String {
    let repeat = match &scenario.config.repeat {
        crate::domain::scenario::RepeatPolicy::Once => "once".to_string(),
        crate::domain::scenario::RepeatPolicy::Every(interval) => {
            format!("every:{}", interval.as_millis())
        }
    };
    let mut material = format!(
        "v{CURRENT_CONTRACT_VERSION}|endpoint={}|rate={:016x}|duration_ms={}|concurrency={}|slices={slices}|startup_burst={global_startup_burst}|max_retries={worker_max_retries}|repeat={repeat}|partition={:?}|execution_semantics={execution_semantics_fingerprint}",
        scenario.config.endpoint,
        scenario.config.scenarios_per_sec.to_bits(),
        scenario.config.duration.as_millis(),
        scenario.config.max_concurrency,
        scenario.config.partition_key_strategy,
    );
    for step in &scenario.steps {
        material.push_str("|step=");
        material.push_str(&step.fingerprint_material());
    }
    format!("fnv128:{:032x}", fnv1a_128(material.as_bytes()))
}

pub fn execution_semantics_fingerprint(
    request_timeout: Duration,
    scenario_timeout: Option<Duration>,
    descriptor_bytes: Option<&[u8]>,
) -> String {
    let scenario_timeout = scenario_timeout
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|| "none".to_string());
    let descriptor = descriptor_bytes
        .map(|bytes| format!("fnv128:{:032x}", fnv1a_128(bytes)))
        .unwrap_or_else(|| "none".to_string());
    let material = format!(
        "engine={}|request_timeout_ms={}|scenario_timeout_ms={scenario_timeout}|descriptor={descriptor}",
        env!("CARGO_PKG_VERSION"),
        request_timeout.as_millis(),
    );
    format!("fnv128:{:032x}", fnv1a_128(material.as_bytes()))
}

/// Verifies that every identity derived by the scheduler remains inside the
/// versioned Kafka contract bound. The timestamp and revision values use their
/// widest representable forms so this remains a startup proof, not a check that
/// can begin failing after the process has already started dispatching.
pub fn validate_scenario_identity_budget(scenario_id: &str) -> Result<(), String> {
    let worst_window = format!(
        "v{CURRENT_CONTRACT_VERSION}:s{}:{scenario_id}:w{}:n{MAX_AUTO_SLICES}",
        scenario_id.len(),
        u128::MAX
    );
    let worst_execution = format!(
        "{worst_window}:slice-{}-of-{MAX_AUTO_SLICES}",
        MAX_AUTO_SLICES - 1
    );
    let worst_result_event =
        build_terminal_event_id(&worst_execution, MAX_CONTRACT_ATTEMPT, "result");
    let worst_summary_event = format!("{worst_window}:summary:r{}:timed-out", u64::MAX);
    for (field, value) in [
        ("scenario_id", scenario_id),
        ("run_id", worst_window.as_str()),
        ("execution_key", worst_execution.as_str()),
        ("result event_id", worst_result_event.as_str()),
        ("summary event_id", worst_summary_event.as_str()),
    ] {
        if value.trim().is_empty() || value.len() > MAX_CONTRACT_ID_BYTES {
            return Err(format!(
                "generated {field} would exceed the {MAX_CONTRACT_ID_BYTES}-byte contract identity bound"
            ));
        }
    }
    Ok(())
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u128::from(*byte)).wrapping_mul(PRIME)
    })
}

fn lease_is_current(
    leader_rx: &watch::Receiver<Option<ObservedLeaderLease>>,
    expected: &ObservedLeaderLease,
) -> bool {
    leader_identity_is_current(leader_rx, expected)
        && leader_rx
            .borrow()
            .as_ref()
            .is_some_and(|current| current.valid_until > Instant::now())
}

fn leader_identity_is_current(
    leader_rx: &watch::Receiver<Option<ObservedLeaderLease>>,
    expected: &ObservedLeaderLease,
) -> bool {
    leader_rx.borrow().as_ref().is_some_and(|current| {
        current.lease.owner_token == expected.lease.owner_token
            && current.lease.fencing_token == expected.lease.fencing_token
    })
}

fn retry_delay(base: Duration, maximum: Duration, attempt: u32, entropy: &str) -> Duration {
    let multiplier = 1_u32 << attempt.min(16);
    let exponential = base.saturating_mul(multiplier).min(maximum);
    let hash = entropy.bytes().fold(
        u64::from(attempt).wrapping_add(0x9e37_79b9),
        |state, byte| {
            state
                .wrapping_mul(1_099_511_628_211)
                .wrapping_add(u64::from(byte))
        },
    );
    // Deterministic 80%-120% jitter keeps tests reproducible and prevents a
    // fleet from synchronizing on identical retry boundaries.
    let basis_points = 8_000_u128 + u128::from(hash % 4_001);
    let jittered_nanos = exponential.as_nanos().saturating_mul(basis_points) / 10_000;
    Duration::from_nanos(u64::try_from(jittered_nanos).unwrap_or(u64::MAX)).min(maximum)
}

fn deferred_job_delay(job: &ScenarioJob, now_unix_ms: u128) -> Duration {
    let remaining_ms = job.not_before_unix_ms.saturating_sub(now_unix_ms);
    Duration::from_millis(u64::try_from(remaining_ms).unwrap_or(u64::MAX))
}

fn status_label(status: &ScenarioRunStatus) -> &'static str {
    match status {
        ScenarioRunStatus::Success => "success",
        ScenarioRunStatus::Failed => "failure",
    }
}

fn scenario_metric_label<'a>(
    plans: &HashMap<String, ScenarioExecutionPlan>,
    scenario_id: &'a str,
) -> &'a str {
    if plans.contains_key(scenario_id) {
        scenario_id
    } else {
        "unknown"
    }
}

fn coordination_error_class(error: &CoordinationError) -> &'static str {
    match error {
        CoordinationError::Unavailable { .. } => "unavailable",
        CoordinationError::Timeout { .. } => "timeout",
        CoordinationError::InvalidState { .. } => "invalid_state",
        CoordinationError::StaleOwner { .. } => "stale_owner",
    }
}

fn shutdown_requested(shutdown_rx: &watch::Receiver<bool>) -> bool {
    *shutdown_rx.borrow()
}

async fn wait_or_shutdown(duration: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        changed = shutdown_rx.changed() => changed.is_err() || shutdown_requested(shutdown_rx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::contracts::PartitionKeyStrategy;
    use crate::domain::error::PulseError;
    use crate::domain::scenario::{RepeatPolicy, ScenarioConfig};

    struct PendingRenewalStore;

    #[async_trait]
    impl ExecutionLeaseStore for PendingRenewalStore {
        async fn claim(&self, _claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
            Err(CoordinationError::invalid_state(
                "test_execution_claim",
                "claim is not used by this test",
            ))
        }

        async fn renew(&self, lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
            sleep(Duration::from_secs(60)).await;
            Ok(lease.clone())
        }

        async fn complete(
            &self,
            _lease: &ExecutionLease,
            _outcome: TerminalOutcome,
        ) -> Result<crate::domain::coordination::CompletedOutcome, CoordinationError> {
            Err(CoordinationError::invalid_state(
                "test_execution_complete",
                "completion is not used by this test",
            ))
        }

        async fn release(
            &self,
            _lease: &ExecutionLease,
        ) -> Result<crate::domain::coordination::ReleaseOutcome, CoordinationError> {
            Ok(crate::domain::coordination::ReleaseOutcome::AlreadyAbsent)
        }
    }

    fn scenario(rate: f64, concurrency: usize) -> Scenario {
        Scenario::new(
            "plan",
            Vec::new(),
            ScenarioConfig {
                endpoint: "http://127.0.0.1:50051".to_string(),
                scenarios_per_sec: rate,
                max_concurrency: concurrency,
                duration: Duration::from_secs(1),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        )
    }

    #[test]
    fn fractional_rate_is_not_floored() {
        assert_eq!(calculate_slices(0.1, 1, Duration::from_secs(10)), 1);
        let scenario = scenario(0.1, 1);
        assert_eq!(slice_load(&scenario, 1, 0, 0).scenarios_per_sec, 0.1);
    }

    #[test]
    fn slice_rates_and_concurrency_sum_to_global_values() {
        for (rate, concurrency) in [(20.0, 3), (200.5, 73), (0.1, 1)] {
            let scenario = scenario(rate, concurrency);
            let slices = calculate_slices(rate, concurrency, scenario.config.duration);
            let loads: Vec<_> = (0..slices)
                .map(|index| slice_load(&scenario, slices, index, 0))
                .collect();
            let total_rate: f64 = loads.iter().map(|load| load.scenarios_per_sec).sum();
            let total_concurrency: usize = loads.iter().map(|load| load.max_concurrency).sum();
            assert!((total_rate - rate).abs() < 1e-9);
            assert_eq!(total_concurrency, concurrency);
            assert!(loads.iter().all(|load| load.max_concurrency > 0));
        }
    }

    #[test]
    fn startup_burst_is_distributed_once_across_slices() {
        let bursts: Vec<_> = (0..4)
            .map(|index| slice_startup_burst(7, 4, index))
            .collect();
        assert_eq!(bursts, vec![2, 2, 2, 1]);
        assert_eq!(bursts.iter().sum::<usize>(), 7);
    }

    #[test]
    fn plan_fingerprint_includes_retry_ceiling() {
        let plan = ScenarioExecutionPlan {
            scenario: scenario(12.0, 4),
            ports: StepPorts {
                default_endpoint: "http://127.0.0.1:50051".to_string(),
                dynamic_grpc_gateways: HashMap::new(),
            },
            execution_semantics_fingerprint: "test-runtime-v1".to_string(),
        };
        assert_ne!(
            execution_plan_fingerprint(&plan, 2, 0, 2),
            execution_plan_fingerprint(&plan, 2, 0, 3),
            "recovered dispatch must reject a changed retry ceiling"
        );

        let mut changed_runtime = plan.clone();
        changed_runtime.execution_semantics_fingerprint = "test-runtime-v2".to_string();
        assert_ne!(
            execution_plan_fingerprint(&plan, 2, 0, 2),
            execution_plan_fingerprint(&changed_runtime, 2, 0, 2),
            "mixed execution semantics must not share a dispatch identity"
        );
    }

    #[test]
    fn execution_semantics_bind_deadlines_and_descriptor_contents() {
        let baseline = execution_semantics_fingerprint(
            Duration::from_secs(5),
            Some(Duration::from_secs(30)),
            Some(b"descriptor-a"),
        );
        assert_ne!(
            baseline,
            execution_semantics_fingerprint(
                Duration::from_secs(6),
                Some(Duration::from_secs(30)),
                Some(b"descriptor-a"),
            )
        );
        assert_ne!(
            baseline,
            execution_semantics_fingerprint(
                Duration::from_secs(5),
                Some(Duration::from_secs(31)),
                Some(b"descriptor-a"),
            )
        );
        assert_ne!(
            baseline,
            execution_semantics_fingerprint(
                Duration::from_secs(5),
                Some(Duration::from_secs(30)),
                Some(b"descriptor-b"),
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn redis_response_latency_consumes_monotonic_lease_budget() {
        let request_started = Instant::now();
        tokio::time::advance(Duration::from_millis(250)).await;

        assert!(
            !lease_response_has_safe_budget(
                request_started,
                Duration::from_millis(300),
                Duration::from_millis(100),
            ),
            "a response arriving after the conservative deadline must fail closed"
        );

        let observed = ObservedLeaderLease::new(
            LeaderLease {
                lock_key: "leader".to_string(),
                node_id: "node-a".to_string(),
                owner_token: "owner-a".to_string(),
                fencing_token: 1,
                expires_at_unix_ms: 1,
                ttl: Duration::from_millis(300),
            },
            Duration::from_millis(100),
            request_started,
        );
        assert!(observed.valid_until <= Instant::now());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_execution_renewal_stops_at_the_current_local_deadline() {
        let started = Instant::now();
        let valid_until = started + Duration::from_millis(25);
        let lease = ExecutionLease {
            execution_key: "deadline-test".to_string(),
            attempt: 0,
            owner_token: "owner-a".to_string(),
            expires_at_unix_ms: 1,
            ttl: Duration::from_millis(100),
            recovered: false,
        };

        let error = lease_renewal_until_loss(
            Arc::new(PendingRenewalStore),
            lease,
            Duration::from_millis(25),
            started,
            valid_until,
        )
        .await;

        assert_eq!(Instant::now(), valid_until);
        assert_eq!(
            error,
            CoordinationError::Timeout {
                operation: "execution_lease_local_deadline",
            }
        );
    }

    #[test]
    fn configured_scenario_name_bound_guarantees_derived_identity_bounds() {
        let scenario_id =
            "s".repeat(crate::application::scenarios::MAX_CONFIGURED_SCENARIO_NAME_BYTES);
        validate_scenario_identity_budget(&scenario_id)
            .expect("maximum configured scenario name must keep every identity bounded");
        assert!(validate_scenario_identity_budget(&"s".repeat(MAX_CONTRACT_ID_BYTES)).is_err());
    }

    #[test]
    fn retry_backoff_is_bounded_and_jittered() {
        let base = Duration::from_secs(1);
        let maximum = Duration::from_secs(30);
        for attempt in 0..20 {
            let delay = retry_delay(base, maximum, attempt, "execution");
            assert!(delay <= maximum);
            assert!(!delay.is_zero());
        }
    }

    #[test]
    fn only_terminal_dispositions_are_committable() {
        assert!(JobDisposition::ResultPublished.is_terminal());
        assert!(JobDisposition::RetryPublished.is_terminal());
        assert!(JobDisposition::DeadLetterPublished.is_terminal());
        assert!(JobDisposition::DuplicateCompleted.is_terminal());
        assert!(
            !JobDisposition::ExecutionLeaseBusy {
                retry_after: Duration::from_secs(1)
            }
            .is_terminal()
        );
        assert!(!JobDisposition::RetryLater.is_terminal());
    }

    #[test]
    fn target_errors_are_measurements_not_infrastructure_retries() {
        let target = PulseError::TargetStatus {
            code: "unavailable".to_string(),
            message: "target rejected request".to_string(),
        };
        assert!(target.is_target_measurement());
        assert!(!target.is_retryable_infrastructure());
    }

    #[test]
    fn failed_job_reason_is_utf8_safe_and_replay_bounded() {
        let oversized = "💥".repeat(MAX_CONTRACT_ID_BYTES);
        let bounded = bounded_failure_reason(oversized);
        assert!(bounded.len() <= MAX_CONTRACT_ID_BYTES);
        assert!(bounded.ends_with("...[truncated]"));
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }
}
