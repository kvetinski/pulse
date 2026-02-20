use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::application::runner::{PulseRunner, RunnerConfig};
use crate::domain::contracts::{
    ErrorCount, FailedScenarioJob, JobLoadConfig, JobSlice, ScenarioJob, ScenarioRunResult,
    ScenarioRunStatus, build_execution_key, now_unix_ms,
};
use crate::domain::scenario::{RepeatPolicy, Scenario, StepPorts};
use crate::infrastructure::metrics as runtime_metrics;

const TARGET_SPS_PER_SLICE: f64 = 10.0;
const TARGET_CONCURRENCY_PER_SLICE: usize = 25;
const MAX_AUTO_SLICES: u32 = 128;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct ScenarioExecutionPlan {
    pub scenario: Scenario,
    pub ports: StepPorts,
}

#[derive(Clone, Debug)]
pub struct NodeRuntimeConfig {
    pub leader_renew_interval: Duration,
    pub scheduler_tick_interval: Duration,
    pub worker_max_retries: u32,
    pub worker_retry_base_delay: Duration,
}

impl Default for NodeRuntimeConfig {
    fn default() -> Self {
        Self {
            leader_renew_interval: Duration::from_secs(3),
            scheduler_tick_interval: Duration::from_millis(500),
            worker_max_retries: 2,
            worker_retry_base_delay: Duration::from_millis(500),
        }
    }
}

#[async_trait]
pub trait LeaderElector: Send + Sync {
    async fn try_acquire_or_renew(&self) -> bool;
    async fn relinquish(&self);
}

#[async_trait]
pub trait DueStateStore: Send + Sync {
    async fn claim_due(&self, scenario_id: &str, repeat: RepeatPolicy) -> bool;
}

#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    async fn claim_once(&self, execution_key: &str) -> bool;
}

#[async_trait]
pub trait JobPublisher: Send + Sync {
    async fn publish_job(&self, key: &str, job: &ScenarioJob) -> Result<(), String>;
}

#[async_trait]
pub trait ResultPublisher: Send + Sync {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String>;
}

#[async_trait]
pub trait DlqPublisher: Send + Sync {
    async fn publish_failed_job(&self, key: &str, job: &FailedScenarioJob) -> Result<(), String>;
}

#[async_trait]
pub trait JobConsumer: Send + Sync {
    type Item: Send;
    async fn recv(&self) -> Result<Option<Self::Item>, String>;
}

pub trait CommitableJob {
    fn job(&self) -> &ScenarioJob;
    fn commit(self) -> Result<(), String>;
}

pub struct PulseNode<E, S, JP, JC, I, RP, DP>
where
    E: LeaderElector,
    S: DueStateStore,
    JP: JobPublisher,
    JC: JobConsumer,
    I: IdempotencyStore,
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
    S: DueStateStore + 'static,
    JP: JobPublisher + 'static,
    JC: JobConsumer + 'static,
    JC::Item: CommitableJob + Send + 'static,
    I: IdempotencyStore + 'static,
    RP: ResultPublisher + 'static,
    DP: DlqPublisher + 'static,
{
    pub fn new(
        elector: Arc<E>,
        due_store: Arc<S>,
        job_publisher: Arc<JP>,
        job_consumer: Arc<JC>,
        idempotency_store: Arc<I>,
        result_publisher: Arc<RP>,
        dlq_publisher: Arc<DP>,
        plans: Vec<ScenarioExecutionPlan>,
        config: NodeRuntimeConfig,
    ) -> Self {
        let map = plans
            .into_iter()
            .map(|plan| (plan.scenario.name.clone(), plan))
            .collect();

        Self {
            elector,
            due_store,
            job_publisher,
            job_consumer,
            idempotency_store,
            result_publisher,
            dlq_publisher,
            plans: Arc::new(map),
            config,
        }
    }

    pub async fn run(self, shutdown_rx: watch::Receiver<bool>) {
        let (leader_tx, leader_rx) = watch::channel(false);
        let mut join_set = JoinSet::new();

        join_set.spawn(leader_election_loop(
            self.elector.clone(),
            leader_tx,
            self.config.leader_renew_interval,
            shutdown_rx.clone(),
        ));

        join_set.spawn(scheduler_loop(
            self.plans.clone(),
            self.due_store.clone(),
            self.job_publisher.clone(),
            leader_rx,
            self.config.scheduler_tick_interval,
            self.config.worker_max_retries,
            shutdown_rx.clone(),
        ));

        join_set.spawn(worker_loop(
            self.plans.clone(),
            self.job_consumer.clone(),
            self.job_publisher.clone(),
            self.idempotency_store.clone(),
            self.result_publisher.clone(),
            self.dlq_publisher.clone(),
            self.config.worker_retry_base_delay,
            shutdown_rx,
        ));

        while let Some(result) = join_set.join_next().await {
            if let Err(err) = result {
                error!(error = %err, "runtime loop exited unexpectedly");
            }
        }
    }
}

async fn leader_election_loop<E: LeaderElector>(
    elector: Arc<E>,
    leader_tx: watch::Sender<bool>,
    renew_interval: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut is_leader = false;

    loop {
        if shutdown_requested(&shutdown_rx) {
            break;
        }

        let currently_leader = elector.try_acquire_or_renew().await;
        if currently_leader != is_leader {
            is_leader = currently_leader;
            let _ = leader_tx.send(is_leader);
            runtime_metrics::set_is_leader(is_leader);
            if is_leader {
                info!("leadership acquired");
            } else {
                warn!("leadership lost");
                elector.relinquish().await;
            }
        }

        tokio::select! {
            _ = sleep(renew_interval) => {}
            _ = shutdown_rx.changed() => {}
        }
    }

    if is_leader {
        elector.relinquish().await;
        runtime_metrics::set_is_leader(false);
        let _ = leader_tx.send(false);
        info!("leadership relinquished on shutdown");
    }
}

async fn scheduler_loop<S: DueStateStore, JP: JobPublisher>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    due_store: Arc<S>,
    publisher: Arc<JP>,
    mut leader_rx: watch::Receiver<bool>,
    tick_interval: Duration,
    worker_max_retries: u32,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if shutdown_requested(&shutdown_rx) {
            info!("scheduler loop stopping");
            return;
        }

        if !*leader_rx.borrow_and_update() {
            tokio::select! {
                changed = leader_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                _ = shutdown_rx.changed() => {
                    if shutdown_requested(&shutdown_rx) {
                        info!("scheduler loop stopping");
                        return;
                    }
                }
            }
            continue;
        }

        for (scenario_id, plan) in plans.iter() {
            let due = due_store
                .claim_due(scenario_id, plan.scenario.config.repeat.clone())
                .await;
            if !due {
                continue;
            }

            let slices = calculate_slices(
                plan.scenario.config.scenarios_per_sec,
                plan.scenario.config.max_concurrency,
            );
            let run_id = Uuid::new_v4().to_string();
            let scheduled_at = now_unix_ms();
            info!(
                scenario = %scenario_id,
                slices,
                scenarios_per_sec = plan.scenario.config.scenarios_per_sec,
                max_concurrency = plan.scenario.config.max_concurrency,
                "scheduling run"
            );

            for index in 0..slices {
                let slice = JobSlice {
                    index,
                    total: slices,
                };
                let execution_key = build_execution_key(scenario_id, scheduled_at, &slice);
                let per_slice_sps = plan.scenario.config.scenarios_per_sec / f64::from(slices);
                let per_slice_concurrency = ((plan.scenario.config.max_concurrency as f64)
                    / f64::from(slices))
                .ceil() as usize;

                let job = ScenarioJob {
                    schema_version: 1,
                    scenario_id: scenario_id.clone(),
                    run_id: run_id.clone(),
                    execution_key,
                    scheduled_at_unix_ms: scheduled_at,
                    slice,
                    load: JobLoadConfig {
                        scenarios_per_sec: per_slice_sps.max(0.1),
                        duration: plan.scenario.config.duration,
                        max_concurrency: per_slice_concurrency.max(1),
                    },
                    attempt: 0,
                    max_retries: worker_max_retries,
                };

                let key = plan.scenario.config.partition_key_strategy.key_for(&job);

                if let Err(err) = publisher.publish_job(&key, &job).await {
                    runtime_metrics::record_scheduler_job_publish_failed(scenario_id);
                    error!(scenario = %scenario_id, error = %err, "failed to publish scenario job");
                } else {
                    runtime_metrics::record_scheduler_job_published(scenario_id);
                    info!(
                        scenario = %scenario_id,
                        run_id = %job.run_id,
                        execution_key = %job.execution_key,
                        slice_index = job.slice.index,
                        slice_total = job.slice.total,
                        partition_strategy = ?plan.scenario.config.partition_key_strategy,
                        "published scenario job"
                    );
                }
            }
        }

        tokio::select! {
            _ = sleep(tick_interval) => {}
            _ = shutdown_rx.changed() => {
                if shutdown_requested(&shutdown_rx) {
                    info!("scheduler loop stopping");
                    return;
                }
            }
        }
    }
}

fn calculate_slices(scenarios_per_sec: f64, max_concurrency: usize) -> u32 {
    let bounded_sps = scenarios_per_sec.max(1.0);
    let bounded_concurrency = max_concurrency.max(1) as u32;

    let slices_by_rate = (bounded_sps / TARGET_SPS_PER_SLICE).ceil() as u32;
    let slices_by_concurrency =
        ((bounded_concurrency as f64) / (TARGET_CONCURRENCY_PER_SLICE as f64)).ceil() as u32;

    // Runner token bucket currently has a 1 scenario/sec floor per slice.
    let max_slices_without_rate_overshoot = bounded_sps.floor() as u32;

    slices_by_rate
        .max(slices_by_concurrency)
        .max(1)
        .min(bounded_concurrency)
        .min(max_slices_without_rate_overshoot.max(1))
        .min(MAX_AUTO_SLICES)
}

#[cfg(test)]
mod tests {
    use super::calculate_slices;

    #[test]
    fn calculates_single_slice_for_low_load() {
        assert_eq!(calculate_slices(1.0, 1), 1);
    }

    #[test]
    fn scales_slices_by_concurrency() {
        assert_eq!(calculate_slices(10.0, 50), 2);
    }

    #[test]
    fn scales_slices_by_rate() {
        assert_eq!(calculate_slices(200.0, 200), 20);
    }

    #[test]
    fn never_exceeds_max_concurrency() {
        assert_eq!(calculate_slices(1_000.0, 5), 5);
    }

    #[test]
    fn enforces_global_upper_bound() {
        assert_eq!(calculate_slices(10_000.0, 1_000), 128);
    }
}

async fn worker_loop<JC, JP, I, RP, DP>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    consumer: Arc<JC>,
    job_publisher: Arc<JP>,
    idempotency_store: Arc<I>,
    result_publisher: Arc<RP>,
    dlq_publisher: Arc<DP>,
    retry_base_delay: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    JC: JobConsumer,
    JC::Item: CommitableJob,
    JP: JobPublisher,
    I: IdempotencyStore,
    RP: ResultPublisher,
    DP: DlqPublisher,
{
    info!("worker loop started");

    loop {
        if shutdown_requested(&shutdown_rx) {
            info!("worker loop stopping");
            return;
        }

        let recv_result = tokio::select! {
            received = consumer.recv() => received,
            _ = shutdown_rx.changed() => {
                if shutdown_requested(&shutdown_rx) {
                    info!("worker loop stopping");
                    return;
                }
                continue;
            }
        };

        let Some(msg) = (match recv_result {
            Ok(v) => v,
            Err(err) => {
                runtime_metrics::record_worker_consume_error();
                error!(error = %err, "failed to consume job");
                tokio::select! {
                    _ = sleep(Duration::from_secs(1)) => {}
                    _ = shutdown_rx.changed() => {
                        if shutdown_requested(&shutdown_rx) {
                            info!("worker loop stopping");
                            return;
                        }
                    }
                }
                continue;
            }
        }) else {
            continue;
        };
        runtime_metrics::record_worker_job_received();

        let job = msg.job().clone();
        let Some(plan) = plans.get(&job.scenario_id) else {
            runtime_metrics::record_worker_unknown_scenario();
            warn!(scenario = %job.scenario_id, "received job for unknown scenario");
            let failed_job = FailedScenarioJob {
                schema_version: 1,
                scenario_id: job.scenario_id.clone(),
                run_id: job.run_id.clone(),
                execution_key: job.execution_key.clone(),
                slice: job.slice.clone(),
                failed_at_unix_ms: now_unix_ms(),
                attempt: job.attempt,
                max_retries: job.max_retries,
                reason: "unknown scenario".to_string(),
            };
            if let Err(err) = publish_to_dlq(&dlq_publisher, &failed_job).await {
                runtime_metrics::record_worker_dlq_publish_failure(&job.scenario_id);
                error!(execution_key = %job.execution_key, error = %err, "failed to publish unknown-scenario job to dlq");
            } else {
                runtime_metrics::record_worker_dlq_published(&job.scenario_id);
            }
            if let Err(err) = msg.commit() {
                runtime_metrics::record_worker_job_commit_failure();
                warn!(error = %err, "failed to commit unknown-scenario job");
            } else {
                runtime_metrics::record_worker_job_commit_success();
            }
            continue;
        };

        let idempotency_key = format!("{}:attempt-{}", job.execution_key, job.attempt);
        if !idempotency_store.claim_once(&idempotency_key).await {
            runtime_metrics::record_worker_duplicate_job();
            info!(
                execution_key = %job.execution_key,
                attempt = job.attempt,
                "skipping duplicate execution"
            );
            if let Err(err) = msg.commit() {
                runtime_metrics::record_worker_job_commit_failure();
                warn!(error = %err, "failed to commit duplicate job");
            } else {
                runtime_metrics::record_worker_job_commit_success();
            }
            continue;
        }

        let started_at = now_unix_ms();
        let report = PulseRunner::run_once(
            plan.scenario.clone(),
            plan.ports.clone(),
            RunnerConfig {
                duration: job.load.duration,
                scenarios_per_sec: job.load.scenarios_per_sec,
                max_concurrency: job.load.max_concurrency,
            },
        )
        .await;
        let finished_at = now_unix_ms();

        let scenario_metrics = report.summary.scenario_metrics.get(&job.scenario_id);
        let status = if report.finished > 0
            && report.finished
                == report
                    .summary
                    .scenario_metrics
                    .get(&job.scenario_id)
                    .map(|m| m.success + m.failure)
                    .unwrap_or(0)
            && report
                .summary
                .scenario_metrics
                .get(&job.scenario_id)
                .map(|m| m.failure == 0)
                .unwrap_or(false)
        {
            ScenarioRunStatus::Success
        } else {
            ScenarioRunStatus::Failed
        };

        let mut error_breakdown: Vec<ErrorCount> = report
            .summary
            .error_counts
            .iter()
            .map(|(kind, count)| ErrorCount {
                kind: kind.clone(),
                count: *count,
            })
            .collect();
        error_breakdown.sort_by(|a, b| a.kind.cmp(&b.kind));

        let result = ScenarioRunResult {
            schema_version: 1,
            scenario_id: job.scenario_id.clone(),
            run_id: job.run_id.clone(),
            execution_key: job.execution_key.clone(),
            slice: job.slice.clone(),
            started_at_unix_ms: started_at,
            finished_at_unix_ms: finished_at,
            status,
            total: report.finished,
            success: scenario_metrics.map(|m| m.success).unwrap_or(0),
            failure: scenario_metrics.map(|m| m.failure).unwrap_or(0),
            scenario_latency_p50_ms: scenario_metrics
                .map(|m| m.latency_ms.value_at_quantile(0.50))
                .unwrap_or(0),
            scenario_latency_p95_ms: scenario_metrics
                .map(|m| m.latency_ms.value_at_quantile(0.95))
                .unwrap_or(0),
            scenario_latency_p99_ms: scenario_metrics
                .map(|m| m.latency_ms.value_at_quantile(0.99))
                .unwrap_or(0),
            error_breakdown,
        };

        if let Err(err) = result_publisher.publish_result(&result).await {
            runtime_metrics::record_worker_result_publish_failure();
            error!(execution_key = %job.execution_key, error = %err, "failed to publish result");
        } else {
            runtime_metrics::record_worker_result_published(
                &result.scenario_id,
                status_label(&result.status),
            );
        }

        if matches!(result.status, ScenarioRunStatus::Failed) {
            let can_retry = job.attempt < job.max_retries;
            if can_retry {
                let mut retry_job = job.clone();
                retry_job.attempt = retry_job.attempt.saturating_add(1);
                let retry_delay = next_retry_delay(retry_base_delay, job.attempt);
                info!(
                    execution_key = %job.execution_key,
                    current_attempt = job.attempt,
                    next_attempt = retry_job.attempt,
                    max_retries = job.max_retries,
                    delay_ms = retry_delay.as_millis(),
                    "scheduling retry for failed scenario job"
                );

                tokio::select! {
                    _ = sleep(retry_delay) => {}
                    _ = shutdown_rx.changed() => {
                        if shutdown_requested(&shutdown_rx) {
                            warn!(
                                execution_key = %job.execution_key,
                                "shutdown before retry publish; sending to dlq"
                            );
                        }
                    }
                }

                if shutdown_requested(&shutdown_rx) {
                    let failed_job = FailedScenarioJob {
                        schema_version: 1,
                        scenario_id: job.scenario_id.clone(),
                        run_id: job.run_id.clone(),
                        execution_key: job.execution_key.clone(),
                        slice: job.slice.clone(),
                        failed_at_unix_ms: now_unix_ms(),
                        attempt: job.attempt,
                        max_retries: job.max_retries,
                        reason: "shutdown before retry publish".to_string(),
                    };
                    if let Err(err) = publish_to_dlq(&dlq_publisher, &failed_job).await {
                        runtime_metrics::record_worker_dlq_publish_failure(&job.scenario_id);
                        error!(execution_key = %job.execution_key, error = %err, "failed to publish shutdown retry fallback to dlq");
                    } else {
                        runtime_metrics::record_worker_dlq_published(&job.scenario_id);
                    }
                    if let Err(err) = msg.commit() {
                        runtime_metrics::record_worker_job_commit_failure();
                        warn!(error = %err, "failed to commit processed job during shutdown");
                    } else {
                        runtime_metrics::record_worker_job_commit_success();
                    }
                    info!("worker loop stopping");
                    return;
                }

                let retry_key = plan.scenario.config.partition_key_strategy.key_for(&retry_job);
                if let Err(err) = job_publisher.publish_job(&retry_key, &retry_job).await {
                    runtime_metrics::record_worker_retry_job_publish_failure(&job.scenario_id);
                    error!(
                        execution_key = %job.execution_key,
                        attempt = retry_job.attempt,
                        error = %err,
                        "failed to publish retry job; sending to dlq"
                    );
                    let failed_job = FailedScenarioJob {
                        schema_version: 1,
                        scenario_id: retry_job.scenario_id.clone(),
                        run_id: retry_job.run_id.clone(),
                        execution_key: retry_job.execution_key.clone(),
                        slice: retry_job.slice.clone(),
                        failed_at_unix_ms: now_unix_ms(),
                        attempt: retry_job.attempt,
                        max_retries: retry_job.max_retries,
                        reason: format!("failed to publish retry: {err}"),
                    };
                    if let Err(dlq_err) = publish_to_dlq(&dlq_publisher, &failed_job).await {
                        runtime_metrics::record_worker_dlq_publish_failure(&job.scenario_id);
                        error!(execution_key = %job.execution_key, error = %dlq_err, "failed to publish retry failure to dlq");
                    } else {
                        runtime_metrics::record_worker_dlq_published(&job.scenario_id);
                    }
                } else {
                    runtime_metrics::record_worker_retry_job_published(&job.scenario_id);
                    info!(
                        execution_key = %job.execution_key,
                        attempt = retry_job.attempt,
                        max_retries = retry_job.max_retries,
                        "published retry job"
                    );
                }
            } else {
                let failed_job = FailedScenarioJob {
                    schema_version: 1,
                    scenario_id: job.scenario_id.clone(),
                    run_id: job.run_id.clone(),
                    execution_key: job.execution_key.clone(),
                    slice: job.slice.clone(),
                    failed_at_unix_ms: now_unix_ms(),
                    attempt: job.attempt,
                    max_retries: job.max_retries,
                    reason: "scenario execution failed and max retries reached".to_string(),
                };
                if let Err(err) = publish_to_dlq(&dlq_publisher, &failed_job).await {
                    runtime_metrics::record_worker_dlq_publish_failure(&job.scenario_id);
                    error!(execution_key = %job.execution_key, error = %err, "failed to publish max-retries failure to dlq");
                } else {
                    runtime_metrics::record_worker_dlq_published(&job.scenario_id);
                    warn!(
                        execution_key = %job.execution_key,
                        attempt = job.attempt,
                        max_retries = job.max_retries,
                        "job moved to dlq after retries exhausted"
                    );
                }
            }
        }

        if let Err(err) = msg.commit() {
            runtime_metrics::record_worker_job_commit_failure();
            warn!(error = %err, "failed to commit processed job");
        } else {
            runtime_metrics::record_worker_job_commit_success();
        }

        info!(
            scenario = %job.scenario_id,
            run_id = %job.run_id,
            execution_key = %job.execution_key,
            attempt = job.attempt,
            max_retries = job.max_retries,
            configured_scenarios_per_sec = report.configured_scenarios_per_sec,
            actual_started_per_sec = report.actual_started_per_sec,
            started = report.started,
            finished = report.finished,
            "worker finished scenario job"
        );
    }
}

fn shutdown_requested(shutdown_rx: &watch::Receiver<bool>) -> bool {
    *shutdown_rx.borrow()
}

fn next_retry_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32 << attempt.min(8);
    base.saturating_mul(multiplier).min(MAX_RETRY_DELAY)
}

async fn publish_to_dlq<DP: DlqPublisher>(
    dlq_publisher: &Arc<DP>,
    failed_job: &FailedScenarioJob,
) -> Result<(), String> {
    dlq_publisher
        .publish_failed_job(&failed_job.execution_key, failed_job)
        .await
}

fn status_label(status: &ScenarioRunStatus) -> &'static str {
    match status {
        ScenarioRunStatus::Success => "success",
        ScenarioRunStatus::Failed => "failure",
    }
}
