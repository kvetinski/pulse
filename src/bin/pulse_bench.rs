use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pulse::application::rate_limiter::TokenBucket;
use pulse::application::runner::{PulseRunner, RunnerConfig};
use pulse::domain::context::ScenarioContext;
use pulse::domain::contracts::PartitionKeyStrategy;
use pulse::domain::error::PulseError;
use pulse::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step, StepPorts};

struct NoopStep;

#[async_trait]
impl Step for NoopStep {
    fn name(&self) -> &str {
        "noop"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        Ok(())
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(default)
}

fn scenarios_per_sec(total: u64, elapsed: Duration) -> f64 {
    total as f64 / elapsed.as_secs_f64().max(0.001)
}

async fn run_token_bucket_benchmark(iterations: u64) -> Duration {
    let mut bucket = TokenBucket::new(2_000.0);
    let start = Instant::now();
    for _ in 0..iterations {
        bucket.acquire().await;
    }
    start.elapsed()
}

fn benchmark_scenario() -> (Scenario, StepPorts, RunnerConfig) {
    let scenario = Scenario::new(
        "BenchScenario",
        vec![Arc::new(NoopStep) as Arc<dyn Step>],
        ScenarioConfig {
            endpoint: "http://127.0.0.1:8080".to_string(),
            scenarios_per_sec: 20.0,
            max_concurrency: 4,
            duration: Duration::from_millis(20),
            repeat: RepeatPolicy::Once,
            partition_key_strategy: PartitionKeyStrategy::ScenarioId,
        },
    );

    let ports = StepPorts {
        default_endpoint: scenario.config.endpoint.clone(),
        dynamic_grpc_gateways: HashMap::new(),
    };

    let config = RunnerConfig {
        duration: scenario.config.duration,
        scenarios_per_sec: scenario.config.scenarios_per_sec,
        max_concurrency: scenario.config.max_concurrency,
    };

    (scenario, ports, config)
}

async fn run_runner_benchmark(iterations: u64) -> (Duration, u64, u64) {
    let (scenario, ports, config) = benchmark_scenario();
    let start = Instant::now();
    let mut total_started = 0_u64;
    let mut total_finished = 0_u64;

    for _ in 0..iterations {
        let report = PulseRunner::run_once(scenario.clone(), ports.clone(), config.clone()).await;
        total_started = total_started.saturating_add(report.started);
        total_finished = total_finished.saturating_add(report.finished);
    }

    (start.elapsed(), total_started, total_finished)
}

#[tokio::main]
async fn main() {
    let token_bucket_iterations = env_u64("PULSE_BENCH_TOKEN_BUCKET_ITERATIONS", 500);
    let runner_iterations = env_u64("PULSE_BENCH_RUNNER_ITERATIONS", 10);
    let min_started_per_sec = env_f64("PULSE_BENCH_MIN_STARTED_PER_SEC", 120.0);
    let max_avg_run_ms = env_f64("PULSE_BENCH_MAX_AVG_RUN_MS", 200.0);
    let max_drop_ratio = env_f64("PULSE_BENCH_MAX_DROP_RATIO", 0.0);

    let token_bucket_elapsed = run_token_bucket_benchmark(token_bucket_iterations).await;
    println!(
        "token_bucket: iterations={} elapsed_ms={} ops_per_sec={:.2}",
        token_bucket_iterations,
        token_bucket_elapsed.as_millis(),
        scenarios_per_sec(token_bucket_iterations, token_bucket_elapsed)
    );

    let (runner_elapsed, started, finished) = run_runner_benchmark(runner_iterations).await;
    let started_per_sec = scenarios_per_sec(started, runner_elapsed);
    let avg_run_ms = (runner_elapsed.as_secs_f64() * 1000.0) / (runner_iterations.max(1) as f64);
    let drop_ratio = if started == 0 {
        1.0
    } else {
        1.0 - (finished as f64 / started as f64)
    };

    println!(
        "runner_noop: runs={} elapsed_ms={} started={} finished={} started_per_sec={:.2}",
        runner_iterations,
        runner_elapsed.as_millis(),
        started,
        finished,
        started_per_sec
    );

    if started_per_sec < min_started_per_sec {
        eprintln!(
            "benchmark regression: started_per_sec {:.2} < min {:.2}",
            started_per_sec, min_started_per_sec
        );
        std::process::exit(1);
    }
    if avg_run_ms > max_avg_run_ms {
        eprintln!(
            "benchmark regression: avg_run_ms {:.2} > max {:.2}",
            avg_run_ms, max_avg_run_ms
        );
        std::process::exit(1);
    }
    if drop_ratio > max_drop_ratio {
        eprintln!(
            "benchmark regression: drop_ratio {:.6} > max {:.6}",
            drop_ratio, max_drop_ratio
        );
        std::process::exit(1);
    }

    println!(
        "bench_thresholds: PASS min_started_per_sec={:.2} max_avg_run_ms={:.2} max_drop_ratio={:.6}",
        min_started_per_sec, max_avg_run_ms, max_drop_ratio
    );
}
