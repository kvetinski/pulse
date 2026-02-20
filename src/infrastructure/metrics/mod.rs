use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::core::Collector;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    TextEncoder,
};
use tracing::{error, info};

static RUNTIME_METRICS: OnceLock<RuntimeMetrics> = OnceLock::new();

pub struct RuntimeMetrics {
    scheduler_jobs_published_total: IntCounterVec,
    scheduler_job_publish_failures_total: IntCounterVec,
    scheduler_is_leader: IntGauge,
    worker_jobs_received_total: IntCounter,
    worker_job_consume_errors_total: IntCounter,
    worker_jobs_unknown_scenario_total: IntCounter,
    worker_jobs_duplicate_total: IntCounter,
    worker_job_commits_total: IntCounter,
    worker_job_commit_failures_total: IntCounter,
    worker_results_published_total: IntCounterVec,
    worker_result_publish_failures_total: IntCounter,
    worker_retry_jobs_published_total: IntCounterVec,
    worker_retry_job_publish_failures_total: IntCounterVec,
    worker_dlq_published_total: IntCounterVec,
    worker_dlq_publish_failures_total: IntCounterVec,
    scenario_inflight: IntGaugeVec,
    scenario_executions_total: IntCounterVec,
    scenario_duration_seconds: HistogramVec,
    step_executions_total: IntCounterVec,
    step_duration_seconds: HistogramVec,
}

impl RuntimeMetrics {
    fn new() -> Self {
        let scheduler_jobs_published_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_scheduler_jobs_published_total",
                "Total number of scenario jobs published by scheduler.",
            ),
            &["scenario"],
        ));
        let scheduler_job_publish_failures_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_scheduler_job_publish_failures_total",
                "Total number of scheduler publish failures.",
            ),
            &["scenario"],
        ));
        let scheduler_is_leader = register(IntGauge::new(
            "pulse_scheduler_is_leader",
            "Whether current node is leader (1=true, 0=false).",
        ));
        let worker_jobs_received_total = register(IntCounter::new(
            "pulse_worker_jobs_received_total",
            "Total number of worker jobs received.",
        ));
        let worker_job_consume_errors_total = register(IntCounter::new(
            "pulse_worker_job_consume_errors_total",
            "Total number of worker job consume errors.",
        ));
        let worker_jobs_unknown_scenario_total = register(IntCounter::new(
            "pulse_worker_jobs_unknown_scenario_total",
            "Total number of jobs skipped because scenario is unknown.",
        ));
        let worker_jobs_duplicate_total = register(IntCounter::new(
            "pulse_worker_jobs_duplicate_total",
            "Total number of duplicate jobs skipped by idempotency store.",
        ));
        let worker_job_commits_total = register(IntCounter::new(
            "pulse_worker_job_commits_total",
            "Total number of worker message commits.",
        ));
        let worker_job_commit_failures_total = register(IntCounter::new(
            "pulse_worker_job_commit_failures_total",
            "Total number of worker message commit failures.",
        ));
        let worker_results_published_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_results_published_total",
                "Total number of scenario run results published.",
            ),
            &["scenario", "status"],
        ));
        let worker_result_publish_failures_total = register(IntCounter::new(
            "pulse_worker_result_publish_failures_total",
            "Total number of result publish failures.",
        ));
        let worker_retry_jobs_published_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_retry_jobs_published_total",
                "Total number of retry jobs published.",
            ),
            &["scenario"],
        ));
        let worker_retry_job_publish_failures_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_retry_job_publish_failures_total",
                "Total number of retry publish failures.",
            ),
            &["scenario"],
        ));
        let worker_dlq_published_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_dlq_published_total",
                "Total number of jobs published to dead-letter topic.",
            ),
            &["scenario"],
        ));
        let worker_dlq_publish_failures_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_dlq_publish_failures_total",
                "Total number of dead-letter publish failures.",
            ),
            &["scenario"],
        ));
        let scenario_inflight = register(IntGaugeVec::new(
            Opts::new(
                "pulse_scenario_inflight",
                "Current number of in-flight scenario executions.",
            ),
            &["scenario"],
        ));
        let scenario_executions_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_scenario_executions_total",
                "Total number of scenario executions.",
            ),
            &["scenario", "status"],
        ));
        let scenario_duration_seconds = register(HistogramVec::new(
            HistogramOpts::new(
                "pulse_scenario_duration_seconds",
                "Scenario execution duration in seconds.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["scenario", "status"],
        ));
        let step_executions_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_step_executions_total",
                "Total number of step executions.",
            ),
            &["scenario", "step", "status"],
        ));
        let step_duration_seconds = register(HistogramVec::new(
            HistogramOpts::new(
                "pulse_step_duration_seconds",
                "Step execution duration in seconds.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["scenario", "step", "status"],
        ));

        let build_info = register(IntGaugeVec::new(
            Opts::new("pulse_build_info", "Build information metric (always 1)."),
            &["version"],
        ))
        .expect("valid metric");
        build_info
            .with_label_values(&[env!("CARGO_PKG_VERSION")])
            .set(1);

        Self {
            scheduler_jobs_published_total: scheduler_jobs_published_total.expect("valid metric"),
            scheduler_job_publish_failures_total: scheduler_job_publish_failures_total
                .expect("valid metric"),
            scheduler_is_leader: scheduler_is_leader.expect("valid metric"),
            worker_jobs_received_total: worker_jobs_received_total.expect("valid metric"),
            worker_job_consume_errors_total: worker_job_consume_errors_total.expect("valid metric"),
            worker_jobs_unknown_scenario_total: worker_jobs_unknown_scenario_total
                .expect("valid metric"),
            worker_jobs_duplicate_total: worker_jobs_duplicate_total.expect("valid metric"),
            worker_job_commits_total: worker_job_commits_total.expect("valid metric"),
            worker_job_commit_failures_total: worker_job_commit_failures_total
                .expect("valid metric"),
            worker_results_published_total: worker_results_published_total.expect("valid metric"),
            worker_result_publish_failures_total: worker_result_publish_failures_total
                .expect("valid metric"),
            worker_retry_jobs_published_total: worker_retry_jobs_published_total
                .expect("valid metric"),
            worker_retry_job_publish_failures_total: worker_retry_job_publish_failures_total
                .expect("valid metric"),
            worker_dlq_published_total: worker_dlq_published_total.expect("valid metric"),
            worker_dlq_publish_failures_total: worker_dlq_publish_failures_total
                .expect("valid metric"),
            scenario_inflight: scenario_inflight.expect("valid metric"),
            scenario_executions_total: scenario_executions_total.expect("valid metric"),
            scenario_duration_seconds: scenario_duration_seconds.expect("valid metric"),
            step_executions_total: step_executions_total.expect("valid metric"),
            step_duration_seconds: step_duration_seconds.expect("valid metric"),
        }
    }
}

fn register<T>(collector: Result<T, prometheus::Error>) -> Result<T, prometheus::Error>
where
    T: Collector + Clone + 'static,
{
    let collector = collector?;
    prometheus::default_registry().register(Box::new(collector.clone()))?;
    Ok(collector)
}

pub fn metrics() -> &'static RuntimeMetrics {
    RUNTIME_METRICS.get_or_init(RuntimeMetrics::new)
}

pub fn set_is_leader(is_leader: bool) {
    metrics()
        .scheduler_is_leader
        .set(if is_leader { 1 } else { 0 });
}

pub fn record_scheduler_job_published(scenario: &str) {
    metrics()
        .scheduler_jobs_published_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_scheduler_job_publish_failed(scenario: &str) {
    metrics()
        .scheduler_job_publish_failures_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_worker_consume_error() {
    metrics().worker_job_consume_errors_total.inc();
}

pub fn record_worker_job_received() {
    metrics().worker_jobs_received_total.inc();
}

pub fn record_worker_unknown_scenario() {
    metrics().worker_jobs_unknown_scenario_total.inc();
}

pub fn record_worker_duplicate_job() {
    metrics().worker_jobs_duplicate_total.inc();
}

pub fn record_worker_job_commit_success() {
    metrics().worker_job_commits_total.inc();
}

pub fn record_worker_job_commit_failure() {
    metrics().worker_job_commit_failures_total.inc();
}

pub fn record_worker_result_published(scenario: &str, status: &str) {
    metrics()
        .worker_results_published_total
        .with_label_values(&[scenario, status])
        .inc();
}

pub fn record_worker_result_publish_failure() {
    metrics().worker_result_publish_failures_total.inc();
}

pub fn record_worker_retry_job_published(scenario: &str) {
    metrics()
        .worker_retry_jobs_published_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_worker_retry_job_publish_failure(scenario: &str) {
    metrics()
        .worker_retry_job_publish_failures_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_worker_dlq_published(scenario: &str) {
    metrics()
        .worker_dlq_published_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_worker_dlq_publish_failure(scenario: &str) {
    metrics()
        .worker_dlq_publish_failures_total
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_scenario_inflight_inc(scenario: &str) {
    metrics()
        .scenario_inflight
        .with_label_values(&[scenario])
        .inc();
}

pub fn record_scenario_inflight_dec(scenario: &str) {
    metrics()
        .scenario_inflight
        .with_label_values(&[scenario])
        .dec();
}

pub fn record_scenario_execution(scenario: &str, duration: Duration, ok: bool) {
    let status = status_label(ok);
    metrics()
        .scenario_executions_total
        .with_label_values(&[scenario, status])
        .inc();
    metrics()
        .scenario_duration_seconds
        .with_label_values(&[scenario, status])
        .observe(duration.as_secs_f64());
}

pub fn record_step_execution(scenario: &str, step: &str, duration: Duration, ok: bool) {
    let status = status_label(ok);
    metrics()
        .step_executions_total
        .with_label_values(&[scenario, step, status])
        .inc();
    metrics()
        .step_duration_seconds
        .with_label_values(&[scenario, step, status])
        .observe(duration.as_secs_f64());
}

pub fn spawn_metrics_server(bind_addr: String) {
    let _ = metrics();

    tokio::spawn(async move {
        let app = Router::new().route("/metrics", get(metrics_handler));
        let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
            Ok(listener) => listener,
            Err(err) => {
                error!(bind_addr = %bind_addr, error = %err, "failed to bind metrics server");
                return;
            }
        };

        info!(bind_addr = %bind_addr, "prometheus metrics server started");
        if let Err(err) = axum::serve(listener, app).await {
            error!(error = %err, "prometheus metrics server exited");
        }
    });
}

async fn metrics_handler() -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    if let Err(err) = encoder.encode(&metric_families, &mut buffer) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("failed to encode metrics: {err}"),
        )
            .into_response();
    }

    match String::from_utf8(buffer) {
        Ok(body) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, encoder.format_type().to_string())],
            body,
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            format!("failed to build metrics response: {err}"),
        )
            .into_response(),
    }
}

fn status_label(ok: bool) -> &'static str {
    if ok { "success" } else { "failure" }
}
