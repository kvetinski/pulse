use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{error, info};

use crate::application::metrics::{GlobalSummary, WorkerMetrics};
use crate::application::rate_limiter::TokenBucket;
use crate::domain::context::ScenarioContext;
use crate::domain::scenario::{RepeatPolicy, Scenario, StepPorts};
use crate::infrastructure::metrics as runtime_metrics;

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub duration: Duration,
    pub scenarios_per_sec: f64,
    pub max_concurrency: usize,
    /// Maximum wall-clock time for one scenario execution. `None` disables the
    /// scenario-level deadline; per-request deadlines still apply.
    pub scenario_timeout: Option<Duration>,
    /// Explicit number of starts allowed at time zero. Zero means strictly
    /// paced startup; every positive value is an intentional initial burst.
    pub startup_burst: usize,
}

pub struct PulseRunner;

impl PulseRunner {
    pub async fn run_service(scenario: Scenario, ports: StepPorts, config: ServiceConfig) {
        let scenario_name = scenario.name.clone();
        loop {
            info!(scenario = %scenario_name, "service cycle started");
            let report =
                Self::run_once(scenario.clone(), ports.clone(), config.runner.clone()).await;
            let summary = report.summary;
            info!(scenario = %scenario_name, "service cycle finished");
            info!(
                scenario = %scenario_name,
                configured_scenarios_per_sec = report.configured_scenarios_per_sec,
                actual_started_per_sec = report.actual_started_per_sec,
                started = report.started,
                finished = report.finished,
                "service cycle throughput"
            );
            summary.print_cli(&scenario_name);

            match config.repeat.clone() {
                RepeatPolicy::Once => {
                    info!(scenario = %scenario_name, "service finished with repeat policy once");
                    return;
                }
                RepeatPolicy::Every(interval) => {
                    info!(
                        scenario = %scenario_name,
                        sleep_secs = interval.as_secs_f64(),
                        "sleeping before next cycle"
                    );
                    sleep(interval).await;
                }
            }
        }
    }

    pub async fn run_once(scenario: Scenario, ports: StepPorts, config: RunnerConfig) -> RunReport {
        info!(
            scenario = %scenario.name,
            duration_secs = config.duration.as_secs_f64(),
            scenarios_per_sec = config.scenarios_per_sec,
            max_concurrency = config.max_concurrency,
            "run started"
        );
        let concurrency = config.max_concurrency.max(1);
        let worker_count = concurrency;
        let workers: Vec<_> = (0..worker_count)
            .map(|_| Arc::new(Mutex::new(WorkerMetrics::new())))
            .collect();

        let mut bucket = if config.startup_burst > 0 {
            TokenBucket::with_burst(config.scenarios_per_sec, config.startup_burst)
        } else {
            TokenBucket::new(config.scenarios_per_sec)
        };
        let mut join_set = JoinSet::new();
        let launch_start = Instant::now();
        let launch_deadline = launch_start + config.duration;
        let mut launched: usize = 0;
        let mut peak_pending_tasks: usize = 0;
        let mut task_failures: u64 = 0;

        'launch: loop {
            task_failures = task_failures.saturating_add(reap_completed(&mut join_set));

            // JoinSet retains completed task metadata until it is joined. Use
            // it as the concurrency gate so both live tasks and retained
            // metadata stay bounded by the configured concurrency.
            while join_set.len() >= concurrency {
                tokio::select! {
                    result = join_set.join_next() => {
                        if let Some(result) = result {
                            task_failures = task_failures
                                .saturating_add(record_join_result(result));
                        }
                    }
                    _ = sleep_until(launch_deadline) => break 'launch,
                }
            }

            if Instant::now() >= launch_deadline {
                break;
            }

            let rate_acquired = loop {
                let has_pending_tasks = !join_set.is_empty();
                tokio::select! {
                    _ = bucket.acquire() => break true,
                    result = join_set.join_next(), if has_pending_tasks => {
                        if let Some(result) = result {
                            task_failures = task_failures
                                .saturating_add(record_join_result(result));
                        }
                    }
                    _ = sleep_until(launch_deadline) => break false,
                }
            };
            if !rate_acquired {
                break;
            }

            // At an exact boundary both the pacing timer and deadline can be
            // ready. The load window is half-open, so never launch at or after
            // its deadline.
            if Instant::now() >= launch_deadline {
                break;
            }

            let worker = workers[launched % worker_count].clone();
            let scenario_clone = scenario.clone();
            let ports_clone = ports.clone();
            let scenario_timeout = config.scenario_timeout;

            join_set.spawn(async move {
                execute_scenario(scenario_clone, ports_clone, worker, scenario_timeout).await;
            });
            launched += 1;
            peak_pending_tasks = peak_pending_tasks.max(join_set.len());
        }

        while let Some(result) = join_set.join_next().await {
            task_failures = task_failures.saturating_add(record_join_result(result));
        }

        let mut summary = GlobalSummary::new();
        for worker in workers {
            let guard = worker.lock().await;
            summary.merge_worker(&guard);
        }
        if task_failures > 0 {
            *summary
                .error_counts
                .entry("invariant_violation".to_string())
                .or_insert(0) += task_failures;
        }

        let launched_u64 = launched as u64;
        let finished = summary
            .scenario_metrics
            .get(&scenario.name)
            .map(|bucket| bucket.total)
            .unwrap_or(0);
        let elapsed_secs = launch_start.elapsed().as_secs_f64().max(0.001);
        let actual_started_per_sec = launched_u64 as f64 / elapsed_secs;

        info!(scenario = %scenario.name, launched, "run completed");

        RunReport {
            summary,
            started: launched_u64,
            finished,
            configured_scenarios_per_sec: config.scenarios_per_sec,
            actual_started_per_sec,
            peak_pending_tasks,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub runner: RunnerConfig,
    pub repeat: RepeatPolicy,
}

pub struct RunReport {
    pub summary: GlobalSummary,
    pub started: u64,
    pub finished: u64,
    pub configured_scenarios_per_sec: f64,
    pub actual_started_per_sec: f64,
    /// Maximum number of task records retained by the runner's `JoinSet`.
    pub peak_pending_tasks: usize,
}

fn reap_completed(join_set: &mut JoinSet<()>) -> u64 {
    let mut failures = 0_u64;
    while let Some(result) = join_set.try_join_next() {
        failures = failures.saturating_add(record_join_result(result));
    }
    failures
}

fn record_join_result(result: Result<(), tokio::task::JoinError>) -> u64 {
    if let Err(err) = result {
        error!(error = %err, "scenario task exited unexpectedly");
        1
    } else {
        0
    }
}

struct ScenarioInflightGuard(String);

impl Drop for ScenarioInflightGuard {
    fn drop(&mut self) {
        runtime_metrics::record_scenario_inflight_dec(&self.0);
    }
}

async fn execute_scenario(
    scenario: Scenario,
    ports: StepPorts,
    worker_metrics: Arc<Mutex<WorkerMetrics>>,
    scenario_timeout: Option<Duration>,
) {
    runtime_metrics::record_scenario_inflight_inc(&scenario.name);
    let _inflight = ScenarioInflightGuard(scenario.name.clone());
    let scenario_start = Instant::now();
    let execution = execute_scenario_inner(&scenario, &ports, &worker_metrics, scenario_start);
    if let Some(deadline) = scenario_timeout {
        if tokio::time::timeout(deadline, execution).await.is_err() {
            let scenario_duration = scenario_start.elapsed();
            let mut metrics = worker_metrics.lock().await;
            metrics.record_error_kind("scenario_timeout".to_string());
            metrics.record_scenario(&scenario.name, scenario_duration, false);
            runtime_metrics::record_scenario_execution(&scenario.name, scenario_duration, false);
            error!(
                scenario = %scenario.name,
                deadline_ms = deadline.as_millis(),
                "scenario execution deadline exceeded"
            );
        }
    } else {
        execution.await;
    }
}

async fn execute_scenario_inner(
    scenario: &Scenario,
    ports: &StepPorts,
    worker_metrics: &Arc<Mutex<WorkerMetrics>>,
    scenario_start: Instant,
) {
    let mut ctx = ScenarioContext::default();

    for step in &scenario.steps {
        let step_start = Instant::now();
        let result = step.execute(&mut ctx, ports).await;
        let step_duration = step_start.elapsed();
        runtime_metrics::record_step_execution(
            &scenario.name,
            step.name(),
            step_duration,
            result.is_ok(),
        );

        {
            let mut metrics = worker_metrics.lock().await;
            metrics.record_step(step.name(), step_duration, result.is_ok());
        }

        if result.is_err()
            && let Err(err) = &result
        {
            error!(
                scenario = %scenario.name,
                step = step.name(),
                error = %err,
                "step execution failed"
            );
            let scenario_duration = scenario_start.elapsed();
            let mut metrics = worker_metrics.lock().await;
            metrics.record_error_kind(err.kind_label());
            metrics.record_scenario(&scenario.name, scenario_duration, false);
            runtime_metrics::record_scenario_execution(&scenario.name, scenario_duration, false);
            return;
        }
    }

    let scenario_duration = scenario_start.elapsed();
    let mut metrics = worker_metrics.lock().await;
    metrics.record_scenario(&scenario.name, scenario_duration, true);
    runtime_metrics::record_scenario_execution(&scenario.name, scenario_duration, true);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;

    use super::{PulseRunner, RunnerConfig};
    use crate::domain::context::ScenarioContext;
    use crate::domain::contracts::PartitionKeyStrategy;
    use crate::domain::error::PulseError;
    use crate::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step, StepPorts};

    struct DelayedStep(Duration);

    #[async_trait]
    impl Step for DelayedStep {
        fn name(&self) -> &str {
            "delayed"
        }

        async fn execute(
            &self,
            _ctx: &mut ScenarioContext,
            _ports: &StepPorts,
        ) -> Result<(), PulseError> {
            tokio::time::sleep(self.0).await;
            Ok(())
        }
    }

    struct PanickingStep;

    #[async_trait]
    impl Step for PanickingStep {
        fn name(&self) -> &str {
            "panicking"
        }

        async fn execute(
            &self,
            _ctx: &mut ScenarioContext,
            _ports: &StepPorts,
        ) -> Result<(), PulseError> {
            panic!("deterministic scenario task panic")
        }
    }

    struct CancellationGuard(Arc<AtomicBool>);

    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct CancellableSlowStep {
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl Step for CancellableSlowStep {
        fn name(&self) -> &str {
            "cancellable-slow"
        }

        async fn execute(
            &self,
            _ctx: &mut ScenarioContext,
            _ports: &StepPorts,
        ) -> Result<(), PulseError> {
            let _guard = CancellationGuard(self.cancelled.clone());
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
    }

    fn test_scenario(name: &str, step_delay: Duration) -> (Scenario, StepPorts) {
        let scenario = Scenario::new(
            name,
            vec![Arc::new(DelayedStep(step_delay)) as Arc<dyn Step>],
            ScenarioConfig {
                endpoint: "http://127.0.0.1:8080".to_string(),
                scenarios_per_sec: 1.0,
                max_concurrency: 1,
                duration: Duration::from_secs(1),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        );
        let ports = StepPorts {
            default_endpoint: scenario.config.endpoint.clone(),
            dynamic_grpc_gateways: HashMap::new(),
        };
        (scenario, ports)
    }

    #[tokio::test(start_paused = true)]
    async fn rate_wait_never_launches_at_or_after_the_deadline() {
        let (scenario, ports) = test_scenario("FractionalDeadline", Duration::ZERO);

        let report = PulseRunner::run_once(
            scenario,
            ports,
            RunnerConfig {
                duration: Duration::from_secs(5),
                scenarios_per_sec: 0.1,
                max_concurrency: 2,
                scenario_timeout: None,
                startup_burst: 0,
            },
        )
        .await;

        assert_eq!(report.started, 0);
        assert_eq!(report.finished, 0);
        assert_eq!(report.peak_pending_tasks, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_wait_never_launches_after_the_deadline() {
        let (scenario, ports) = test_scenario("ConcurrencyDeadline", Duration::from_secs(10));

        let report = PulseRunner::run_once(
            scenario,
            ports,
            RunnerConfig {
                duration: Duration::from_secs(1),
                scenarios_per_sec: 100.0,
                max_concurrency: 1,
                scenario_timeout: None,
                startup_burst: 0,
            },
        )
        .await;

        assert_eq!(report.started, 1);
        assert_eq!(report.finished, 1);
        assert_eq!(report.peak_pending_tasks, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn continuously_reaps_tasks_and_bounds_pending_metadata() {
        let concurrency = 3;
        let (scenario, ports) = test_scenario("BoundedJoinSet", Duration::from_millis(100));

        let report = PulseRunner::run_once(
            scenario,
            ports,
            RunnerConfig {
                duration: Duration::from_secs(1),
                scenarios_per_sec: 1_000.0,
                max_concurrency: concurrency,
                scenario_timeout: None,
                startup_burst: 0,
            },
        )
        .await;

        assert!(report.started > concurrency as u64);
        assert_eq!(report.started, report.finished);
        assert_eq!(report.peak_pending_tasks, concurrency);
    }

    #[tokio::test(start_paused = true)]
    async fn task_panic_is_reported_as_an_internal_invariant_violation() {
        let scenario = Scenario::new(
            "PanickingScenario",
            vec![Arc::new(PanickingStep) as Arc<dyn Step>],
            ScenarioConfig {
                endpoint: "http://127.0.0.1:8080".to_string(),
                scenarios_per_sec: 1.0,
                max_concurrency: 1,
                duration: Duration::from_secs(1),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        );
        let ports = StepPorts {
            default_endpoint: scenario.config.endpoint.clone(),
            dynamic_grpc_gateways: HashMap::new(),
        };

        let report = PulseRunner::run_once(
            scenario,
            ports,
            RunnerConfig {
                duration: Duration::from_secs(1),
                scenarios_per_sec: 1.0,
                max_concurrency: 1,
                scenario_timeout: None,
                startup_burst: 1,
            },
        )
        .await;

        assert_eq!(report.started, 1);
        assert_eq!(report.finished, 0);
        assert_eq!(
            report.summary.error_counts.get("invariant_violation"),
            Some(&1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scenario_deadline_cancels_a_slow_step_and_records_a_measurement() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let scenario = Scenario::new(
            "ScenarioDeadline",
            vec![Arc::new(CancellableSlowStep {
                cancelled: cancelled.clone(),
            }) as Arc<dyn Step>],
            ScenarioConfig {
                endpoint: "http://127.0.0.1:8080".to_string(),
                scenarios_per_sec: 0.1,
                max_concurrency: 1,
                duration: Duration::from_secs(1),
                repeat: RepeatPolicy::Once,
                partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
            },
        );
        let ports = StepPorts {
            default_endpoint: scenario.config.endpoint.clone(),
            dynamic_grpc_gateways: HashMap::new(),
        };

        let report = PulseRunner::run_once(
            scenario,
            ports,
            RunnerConfig {
                duration: Duration::from_secs(1),
                scenarios_per_sec: 0.1,
                max_concurrency: 1,
                scenario_timeout: Some(Duration::from_secs(5)),
                startup_burst: 1,
            },
        )
        .await;

        assert!(cancelled.load(Ordering::SeqCst));
        assert_eq!(report.started, 1);
        assert_eq!(report.finished, 1);
        assert_eq!(
            report.summary.error_counts.get("scenario_timeout"),
            Some(&1)
        );
        let scenario_metrics = report
            .summary
            .scenario_metrics
            .get("ScenarioDeadline")
            .expect("scenario timeout measurement");
        assert_eq!(scenario_metrics.failure, 1);
    }
}
