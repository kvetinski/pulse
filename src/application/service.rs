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
    ErrorCount, JobLoadConfig, JobSlice, ScenarioJob, ScenarioRunResult, ScenarioRunStatus,
    build_execution_key, now_unix_ms,
};
use crate::domain::scenario::{RepeatPolicy, Scenario, StepPorts};

const TARGET_SPS_PER_SLICE: f64 = 10.0;
const TARGET_CONCURRENCY_PER_SLICE: usize = 25;
const MAX_AUTO_SLICES: u32 = 128;

#[derive(Clone)]
pub struct ScenarioExecutionPlan {
    pub scenario: Scenario,
    pub ports: StepPorts,
}

#[derive(Clone, Debug)]
pub struct NodeRuntimeConfig {
    pub leader_renew_interval: Duration,
    pub scheduler_tick_interval: Duration,
}

impl Default for NodeRuntimeConfig {
    fn default() -> Self {
        Self {
            leader_renew_interval: Duration::from_secs(3),
            scheduler_tick_interval: Duration::from_millis(500),
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
pub trait JobConsumer: Send + Sync {
    type Item: Send;
    async fn recv(&self) -> Result<Option<Self::Item>, String>;
}

pub trait CommitableJob {
    fn job(&self) -> &ScenarioJob;
    fn commit(self) -> Result<(), String>;
}

pub struct PulseNode<E, S, JP, JC, I, RP>
where
    E: LeaderElector,
    S: DueStateStore,
    JP: JobPublisher,
    JC: JobConsumer,
    I: IdempotencyStore,
    RP: ResultPublisher,
{
    elector: Arc<E>,
    due_store: Arc<S>,
    job_publisher: Arc<JP>,
    job_consumer: Arc<JC>,
    idempotency_store: Arc<I>,
    result_publisher: Arc<RP>,
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    config: NodeRuntimeConfig,
}

impl<E, S, JP, JC, I, RP> PulseNode<E, S, JP, JC, I, RP>
where
    E: LeaderElector + 'static,
    S: DueStateStore + 'static,
    JP: JobPublisher + 'static,
    JC: JobConsumer + 'static,
    JC::Item: CommitableJob + Send + 'static,
    I: IdempotencyStore + 'static,
    RP: ResultPublisher + 'static,
{
    pub fn new(
        elector: Arc<E>,
        due_store: Arc<S>,
        job_publisher: Arc<JP>,
        job_consumer: Arc<JC>,
        idempotency_store: Arc<I>,
        result_publisher: Arc<RP>,
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
            plans: Arc::new(map),
            config,
        }
    }

    pub async fn run(self) {
        let (leader_tx, leader_rx) = watch::channel(false);
        let mut join_set = JoinSet::new();

        join_set.spawn(leader_election_loop(
            self.elector.clone(),
            leader_tx,
            self.config.leader_renew_interval,
        ));

        join_set.spawn(scheduler_loop(
            self.plans.clone(),
            self.due_store.clone(),
            self.job_publisher.clone(),
            leader_rx,
            self.config.scheduler_tick_interval,
        ));

        join_set.spawn(worker_loop(
            self.plans.clone(),
            self.job_consumer.clone(),
            self.idempotency_store.clone(),
            self.result_publisher.clone(),
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
) {
    let mut is_leader = false;

    loop {
        let currently_leader = elector.try_acquire_or_renew().await;
        if currently_leader != is_leader {
            is_leader = currently_leader;
            let _ = leader_tx.send(is_leader);
            if is_leader {
                info!("leadership acquired");
            } else {
                warn!("leadership lost");
                elector.relinquish().await;
            }
        }

        sleep(renew_interval).await;
    }
}

async fn scheduler_loop<S: DueStateStore, JP: JobPublisher>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    due_store: Arc<S>,
    publisher: Arc<JP>,
    mut leader_rx: watch::Receiver<bool>,
    tick_interval: Duration,
) {
    loop {
        if !*leader_rx.borrow_and_update() {
            if leader_rx.changed().await.is_err() {
                return;
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
                };

                let key = plan.scenario.config.partition_key_strategy.key_for(&job);

                if let Err(err) = publisher.publish_job(&key, &job).await {
                    error!(scenario = %scenario_id, error = %err, "failed to publish scenario job");
                } else {
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

        sleep(tick_interval).await;
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

async fn worker_loop<JC, I, RP>(
    plans: Arc<HashMap<String, ScenarioExecutionPlan>>,
    consumer: Arc<JC>,
    idempotency_store: Arc<I>,
    result_publisher: Arc<RP>,
) where
    JC: JobConsumer,
    JC::Item: CommitableJob,
    I: IdempotencyStore,
    RP: ResultPublisher,
{
    info!("worker loop started");

    loop {
        let Some(msg) = (match consumer.recv().await {
            Ok(v) => v,
            Err(err) => {
                error!(error = %err, "failed to consume job");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }) else {
            continue;
        };

        let job = msg.job().clone();
        let Some(plan) = plans.get(&job.scenario_id) else {
            warn!(scenario = %job.scenario_id, "received job for unknown scenario");
            if let Err(err) = msg.commit() {
                warn!(error = %err, "failed to commit unknown-scenario job");
            }
            continue;
        };

        if !idempotency_store.claim_once(&job.execution_key).await {
            info!(execution_key = %job.execution_key, "skipping duplicate execution");
            if let Err(err) = msg.commit() {
                warn!(error = %err, "failed to commit duplicate job");
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
            error!(execution_key = %job.execution_key, error = %err, "failed to publish result");
        }

        if let Err(err) = msg.commit() {
            warn!(error = %err, "failed to commit processed job");
        }

        info!(
            scenario = %job.scenario_id,
            run_id = %job.run_id,
            execution_key = %job.execution_key,
            configured_scenarios_per_sec = report.configured_scenarios_per_sec,
            actual_started_per_sec = report.actual_started_per_sec,
            started = report.started,
            finished = report.finished,
            "worker finished scenario job"
        );
    }
}
