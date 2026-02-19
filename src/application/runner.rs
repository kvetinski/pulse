use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{error, info};

use crate::application::metrics::{GlobalSummary, WorkerMetrics};
use crate::application::rate_limiter::TokenBucket;
use crate::domain::context::ScenarioContext;
use crate::domain::scenario::{RepeatPolicy, Scenario, StepPorts};

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub duration: Duration,
    pub scenarios_per_sec: f64,
    pub max_concurrency: usize,
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

        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut bucket = TokenBucket::new(config.scenarios_per_sec);
        let mut join_set = JoinSet::new();
        let launch_start = Instant::now();
        let mut launched: usize = 0;

        while launch_start.elapsed() < config.duration {
            bucket.acquire().await;

            if launch_start.elapsed() >= config.duration {
                break;
            }

            let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
            let worker = workers[launched % worker_count].clone();
            let scenario_clone = scenario.clone();
            let ports_clone = ports.clone();

            join_set.spawn(async move {
                let _permit = permit;
                execute_scenario(scenario_clone, ports_clone, worker).await;
            });
            launched += 1;
        }

        while join_set.join_next().await.is_some() {}

        let mut summary = GlobalSummary::new();
        for worker in workers {
            let guard = worker.lock().await;
            summary.merge_worker(&guard);
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
}

async fn execute_scenario(
    scenario: Scenario,
    ports: StepPorts,
    worker_metrics: Arc<Mutex<WorkerMetrics>>,
) {
    let scenario_start = Instant::now();
    let mut ctx = ScenarioContext::default();

    for step in &scenario.steps {
        let step_start = Instant::now();
        let result = step.execute(&mut ctx, &ports).await;
        let step_duration = step_start.elapsed();

        {
            let mut metrics = worker_metrics.lock().await;
            metrics.record_step(step.name(), step_duration, result.is_ok());
        }

        if result.is_err() {
            if let Err(err) = &result {
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
                return;
            }
        }
    }

    let scenario_duration = scenario_start.elapsed();
    let mut metrics = worker_metrics.lock().await;
    metrics.record_scenario(&scenario.name, scenario_duration, true);
}
