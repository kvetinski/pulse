use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use prometheus::core::Collector;
use prometheus::{
    Encoder, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge,
    IntGaugeVec, Opts, TextEncoder,
};
use tracing::{error, info};

static RUNTIME_METRICS: OnceLock<RuntimeMetrics> = OnceLock::new();

/// Shared health state for the metrics and health server.
///
/// Readiness fails closed: every required startup milestone must be explicitly
/// marked ready, and beginning a drain immediately makes the process unready.
/// Clones share the same atomic state and are cheap to pass to runtime loops.
#[derive(Clone, Default)]
pub struct HealthState {
    inner: Arc<HealthStateInner>,
}

#[derive(Default)]
struct HealthStateInner {
    config_loaded: AtomicBool,
    redis_ready: AtomicBool,
    kafka_topics_ready: AtomicBool,
    kafka_producers_ready: AtomicBool,
    kafka_consumer_ready: AtomicBool,
    scenarios_initialized: AtomicBool,
    worker_accepting: AtomicBool,
    draining: AtomicBool,
}

/// A point-in-time view of the runtime readiness prerequisites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthSnapshot {
    pub config_loaded: bool,
    pub redis_ready: bool,
    pub kafka_topics_ready: bool,
    pub kafka_producers_ready: bool,
    pub kafka_consumer_ready: bool,
    pub scenarios_initialized: bool,
    pub worker_accepting: bool,
    pub draining: bool,
}

impl HealthState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_config_loaded(&self, ready: bool) {
        self.inner.config_loaded.store(ready, Ordering::Release);
    }

    pub fn set_redis_ready(&self, ready: bool) {
        self.inner.redis_ready.store(ready, Ordering::Release);
    }

    pub fn set_kafka_topics_ready(&self, ready: bool) {
        self.inner
            .kafka_topics_ready
            .store(ready, Ordering::Release);
    }

    pub fn set_kafka_producers_ready(&self, ready: bool) {
        self.inner
            .kafka_producers_ready
            .store(ready, Ordering::Release);
    }

    pub fn set_kafka_consumer_ready(&self, ready: bool) {
        self.inner
            .kafka_consumer_ready
            .store(ready, Ordering::Release);
    }

    pub fn set_scenarios_initialized(&self, ready: bool) {
        self.inner
            .scenarios_initialized
            .store(ready, Ordering::Release);
    }

    pub fn set_worker_accepting(&self, ready: bool) {
        self.inner.worker_accepting.store(ready, Ordering::Release);
    }

    pub fn set_draining(&self, draining: bool) {
        self.inner.draining.store(draining, Ordering::Release);
    }

    /// Marks shutdown draining as started and stops advertising worker capacity.
    pub fn begin_draining(&self) {
        self.set_worker_accepting(false);
        self.set_draining(true);
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            config_loaded: self.inner.config_loaded.load(Ordering::Acquire),
            redis_ready: self.inner.redis_ready.load(Ordering::Acquire),
            kafka_topics_ready: self.inner.kafka_topics_ready.load(Ordering::Acquire),
            kafka_producers_ready: self.inner.kafka_producers_ready.load(Ordering::Acquire),
            kafka_consumer_ready: self.inner.kafka_consumer_ready.load(Ordering::Acquire),
            scenarios_initialized: self.inner.scenarios_initialized.load(Ordering::Acquire),
            worker_accepting: self.inner.worker_accepting.load(Ordering::Acquire),
            draining: self.inner.draining.load(Ordering::Acquire),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.snapshot().is_ready()
    }
}

impl HealthSnapshot {
    pub fn is_ready(self) -> bool {
        self.config_loaded
            && self.redis_ready
            && self.kafka_topics_ready
            && self.kafka_producers_ready
            && self.kafka_consumer_ready
            && self.scenarios_initialized
            && self.worker_accepting
            && !self.draining
    }
}

pub struct RuntimeMetrics {
    scheduler_jobs_published_total: IntCounterVec,
    scheduler_job_publish_failures_total: IntCounterVec,
    scheduler_is_leader: IntGauge,
    scheduler_leadership_changes_total: IntCounterVec,
    scheduler_leadership_renewal_failures_total: IntCounterVec,
    scheduler_incomplete_dispatch_slices: IntGaugeVec,
    scheduler_schedule_lag_seconds: HistogramVec,
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
    worker_execution_lease_total: IntCounterVec,
    worker_execution_lease_renewal_failures_total: IntCounterVec,
    worker_uncommitted_jobs: IntGauge,
    worker_retry_queue_depth: IntGauge,
    worker_job_age_seconds: Histogram,
    worker_retry_job_age_seconds: Histogram,
    worker_processing_duration_seconds: Histogram,
    worker_kafka_publish_duration_seconds: HistogramVec,
    worker_kafka_commit_duration_seconds: Histogram,
    worker_shutdown_drain_duration_seconds: Histogram,
    aggregate_results_total: IntCounterVec,
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
        let scheduler_leadership_changes_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_scheduler_leadership_changes_total",
                "Leadership state transitions, labelled by the bounded transition outcome.",
            ),
            &["outcome"],
        ));
        let scheduler_leadership_renewal_failures_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_scheduler_leadership_renewal_failures_total",
                "Leader lease renewal failures by bounded error class.",
            ),
            &["class"],
        ));
        let scheduler_incomplete_dispatch_slices = register(IntGaugeVec::new(
            Opts::new(
                "pulse_scheduler_incomplete_dispatch_slices",
                "Current unacknowledged deterministic slices in a dispatch window.",
            ),
            &["scenario"],
        ));
        let scheduler_schedule_lag_seconds = register(HistogramVec::new(
            HistogramOpts::new(
                "pulse_scheduler_schedule_lag_seconds",
                "Delay between a deterministic schedule window and scheduler processing.",
            )
            .buckets(vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 15.0, 60.0]),
            &["scenario"],
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
        let worker_execution_lease_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_execution_lease_total",
                "Execution lease outcomes by bounded outcome class.",
            ),
            &["outcome"],
        ));
        let worker_execution_lease_renewal_failures_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_worker_execution_lease_renewal_failures_total",
                "Execution lease renewal failures by bounded error class.",
            ),
            &["class"],
        ));
        let worker_uncommitted_jobs = register(IntGauge::new(
            "pulse_worker_uncommitted_jobs",
            "Kafka jobs consumed by this process but not synchronously committed.",
        ));
        let worker_retry_queue_depth = register(IntGauge::new(
            "pulse_worker_retry_queue_depth",
            "Current bounded in-process settlement retries (at most one per worker).",
        ));
        let worker_job_age_seconds = register(Histogram::with_opts(
            HistogramOpts::new(
                "pulse_worker_job_age_seconds",
                "Age of a job when processing begins.",
            )
            .buckets(vec![0.01, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1_800.0]),
        ));
        let worker_retry_job_age_seconds = register(Histogram::with_opts(
            HistogramOpts::new(
                "pulse_worker_retry_job_age_seconds",
                "Age since the original schedule window when a retry attempt begins processing.",
            )
            .buckets(vec![0.01, 0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 1_800.0]),
        ));
        let worker_processing_duration_seconds = register(Histogram::with_opts(
            HistogramOpts::new(
                "pulse_worker_processing_duration_seconds",
                "End-to-end job processing and terminal-settlement duration.",
            )
            .buckets(vec![
                0.01, 0.1, 0.5, 1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0,
            ]),
        ));
        let worker_kafka_publish_duration_seconds = register(HistogramVec::new(
            HistogramOpts::new(
                "pulse_kafka_publish_duration_seconds",
                "Kafka producer acknowledgement latency by bounded record kind.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0, 10.0,
            ]),
            &["kind", "outcome"],
        ));
        let worker_kafka_commit_duration_seconds = register(Histogram::with_opts(
            HistogramOpts::new(
                "pulse_kafka_commit_duration_seconds",
                "Synchronous Kafka source-offset commit latency.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0,
            ]),
        ));
        let worker_shutdown_drain_duration_seconds = register(Histogram::with_opts(
            HistogramOpts::new(
                "pulse_shutdown_drain_duration_seconds",
                "Worker shutdown drain duration.",
            )
            .buckets(vec![0.01, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0]),
        ));
        let aggregate_results_total = register(IntCounterVec::new(
            Opts::new(
                "pulse_aggregate_results_total",
                "Run aggregation updates by bounded outcome.",
            ),
            &["outcome"],
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
            scheduler_leadership_changes_total: scheduler_leadership_changes_total
                .expect("valid metric"),
            scheduler_leadership_renewal_failures_total:
                scheduler_leadership_renewal_failures_total.expect("valid metric"),
            scheduler_incomplete_dispatch_slices: scheduler_incomplete_dispatch_slices
                .expect("valid metric"),
            scheduler_schedule_lag_seconds: scheduler_schedule_lag_seconds.expect("valid metric"),
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
            worker_execution_lease_total: worker_execution_lease_total.expect("valid metric"),
            worker_execution_lease_renewal_failures_total:
                worker_execution_lease_renewal_failures_total.expect("valid metric"),
            worker_uncommitted_jobs: worker_uncommitted_jobs.expect("valid metric"),
            worker_retry_queue_depth: worker_retry_queue_depth.expect("valid metric"),
            worker_job_age_seconds: worker_job_age_seconds.expect("valid metric"),
            worker_retry_job_age_seconds: worker_retry_job_age_seconds.expect("valid metric"),
            worker_processing_duration_seconds: worker_processing_duration_seconds
                .expect("valid metric"),
            worker_kafka_publish_duration_seconds: worker_kafka_publish_duration_seconds
                .expect("valid metric"),
            worker_kafka_commit_duration_seconds: worker_kafka_commit_duration_seconds
                .expect("valid metric"),
            worker_shutdown_drain_duration_seconds: worker_shutdown_drain_duration_seconds
                .expect("valid metric"),
            aggregate_results_total: aggregate_results_total.expect("valid metric"),
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

pub fn record_leadership_change(outcome: &'static str) {
    metrics()
        .scheduler_leadership_changes_total
        .with_label_values(&[outcome])
        .inc();
}

pub fn record_leadership_renewal_failure(class: &'static str) {
    metrics()
        .scheduler_leadership_renewal_failures_total
        .with_label_values(&[class])
        .inc();
}

pub fn set_incomplete_dispatch_slices(scenario: &str, count: u32) {
    metrics()
        .scheduler_incomplete_dispatch_slices
        .with_label_values(&[scenario])
        .set(i64::from(count));
}

pub fn observe_schedule_lag(scenario: &str, lag: Duration) {
    metrics()
        .scheduler_schedule_lag_seconds
        .with_label_values(&[scenario])
        .observe(lag.as_secs_f64());
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
    metrics().worker_uncommitted_jobs.inc();
}

pub fn record_worker_unknown_scenario() {
    metrics().worker_jobs_unknown_scenario_total.inc();
}

pub fn record_worker_duplicate_job() {
    metrics().worker_jobs_duplicate_total.inc();
}

pub fn record_worker_job_commit_success() {
    metrics().worker_job_commits_total.inc();
    metrics().worker_uncommitted_jobs.dec();
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

pub fn record_execution_lease(outcome: &'static str) {
    metrics()
        .worker_execution_lease_total
        .with_label_values(&[outcome])
        .inc();
}

pub fn record_execution_lease_renewal_failure(class: &'static str) {
    metrics()
        .worker_execution_lease_renewal_failures_total
        .with_label_values(&[class])
        .inc();
}

pub fn set_retry_queue_depth(depth: i64) {
    metrics().worker_retry_queue_depth.set(depth.clamp(0, 1));
}

pub fn observe_job_age(age: Duration) {
    metrics().worker_job_age_seconds.observe(age.as_secs_f64());
}

pub fn observe_retry_job_age(age: Duration) {
    metrics()
        .worker_retry_job_age_seconds
        .observe(age.as_secs_f64());
}

pub fn observe_job_processing(duration: Duration) {
    metrics()
        .worker_processing_duration_seconds
        .observe(duration.as_secs_f64());
}

pub fn observe_kafka_publish(kind: &'static str, duration: Duration, ok: bool) {
    metrics()
        .worker_kafka_publish_duration_seconds
        .with_label_values(&[kind, if ok { "acknowledged" } else { "failed" }])
        .observe(duration.as_secs_f64());
}

pub fn observe_kafka_commit(duration: Duration) {
    metrics()
        .worker_kafka_commit_duration_seconds
        .observe(duration.as_secs_f64());
}

pub fn observe_shutdown_drain(duration: Duration) {
    metrics()
        .worker_shutdown_drain_duration_seconds
        .observe(duration.as_secs_f64());
}

pub fn record_aggregation_update(outcome: &'static str) {
    metrics()
        .aggregate_results_total
        .with_label_values(&[outcome])
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
    // Compatibility for existing callers. Readiness intentionally remains
    // false until the caller adopts `spawn_metrics_server_with_health` and
    // explicitly reports runtime milestones.
    drop(spawn_metrics_server_with_health(
        bind_addr,
        HealthState::new(),
    ));
}

/// Starts the metrics and health server with caller-managed readiness state.
///
/// The returned task handle lets the runtime observe an unexpected server
/// exit. Binding happens in the task; callers that need startup errors to be
/// synchronous can run [`serve_metrics_and_health`] directly.
pub fn spawn_metrics_server_with_health(
    bind_addr: String,
    health: HealthState,
) -> tokio::task::JoinHandle<()> {
    let _ = metrics();

    tokio::spawn(async move {
        if let Err(err) = serve_metrics_and_health(&bind_addr, health).await {
            error!(bind_addr = %bind_addr, error = %err, "metrics and health server exited");
        }
    })
}

/// Serves Prometheus metrics and independent live/ready health endpoints.
pub async fn serve_metrics_and_health(bind_addr: &str, health: HealthState) -> std::io::Result<()> {
    let _ = metrics();
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    info!(bind_addr = %local_addr, "metrics and health server started");
    axum::serve(listener, health_router(health)).await
}

/// Builds the server router. Exposed for embedding and focused endpoint tests.
pub fn health_router(health: HealthState) -> Router {
    Router::new()
        .route("/health/live", get(liveness_handler))
        .route("/health/ready", get(readiness_handler))
        .route("/metrics", get(metrics_handler))
        .with_state(health)
}

async fn liveness_handler() -> impl IntoResponse {
    health_response(StatusCode::OK, "live\n")
}

async fn readiness_handler(State(health): State<HealthState>) -> impl IntoResponse {
    let snapshot = health.snapshot();
    if snapshot.is_ready() {
        return health_response(StatusCode::OK, "ready\n");
    }

    health_response(
        StatusCode::SERVICE_UNAVAILABLE,
        readiness_diagnostic(snapshot),
    )
}

fn health_response(status: StatusCode, body: impl Into<String>) -> axum::response::Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

fn readiness_diagnostic(snapshot: HealthSnapshot) -> String {
    let prerequisites = [
        ("config", snapshot.config_loaded),
        ("redis", snapshot.redis_ready),
        ("kafka_topics", snapshot.kafka_topics_ready),
        ("kafka_producers", snapshot.kafka_producers_ready),
        ("kafka_consumer", snapshot.kafka_consumer_ready),
        ("scenarios", snapshot.scenarios_initialized),
        ("worker", snapshot.worker_accepting),
    ];
    let missing = prerequisites
        .into_iter()
        .filter_map(|(name, ready)| (!ready).then_some(name))
        .collect::<Vec<_>>()
        .join(",");

    match (missing.is_empty(), snapshot.draining) {
        (true, true) => "not ready: draining\n".to_owned(),
        (false, true) => format!("not ready: missing={missing}; draining\n"),
        (false, false) => format!("not ready: missing={missing}\n"),
        (true, false) => "ready\n".to_owned(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mark_all_ready(health: &HealthState) {
        health.set_config_loaded(true);
        health.set_redis_ready(true);
        health.set_kafka_topics_ready(true);
        health.set_kafka_producers_ready(true);
        health.set_kafka_consumer_ready(true);
        health.set_scenarios_initialized(true);
        health.set_worker_accepting(true);
    }

    #[test]
    fn readiness_fails_closed_and_reports_missing_requirements() {
        let health = HealthState::new();

        assert!(!health.is_ready());
        assert_eq!(
            readiness_diagnostic(health.snapshot()),
            "not ready: missing=config,redis,kafka_topics,kafka_producers,kafka_consumer,scenarios,worker\n"
        );
    }

    #[test]
    fn readiness_requires_every_runtime_milestone() {
        let health = HealthState::new();
        mark_all_ready(&health);

        assert!(health.is_ready());
        assert_eq!(readiness_diagnostic(health.snapshot()), "ready\n");

        health.set_redis_ready(false);
        assert!(!health.is_ready());
        assert_eq!(
            readiness_diagnostic(health.snapshot()),
            "not ready: missing=redis\n"
        );

        health.set_redis_ready(true);
        health.set_kafka_topics_ready(false);
        assert!(!health.is_ready());
        assert_eq!(
            readiness_diagnostic(health.snapshot()),
            "not ready: missing=kafka_topics\n"
        );
    }

    #[test]
    fn clones_share_state_and_draining_overrides_readiness() {
        let health = HealthState::new();
        let runtime_health = health.clone();
        mark_all_ready(&runtime_health);

        assert!(health.is_ready());

        runtime_health.begin_draining();
        assert!(!health.is_ready());
        assert_eq!(
            readiness_diagnostic(health.snapshot()),
            "not ready: missing=worker; draining\n"
        );
    }

    #[test]
    fn readiness_diagnostic_is_bounded() {
        let diagnostic = readiness_diagnostic(HealthState::new().snapshot());

        assert!(diagnostic.len() <= 128);
    }

    #[tokio::test]
    async fn health_handlers_return_independent_statuses() {
        let health = HealthState::new();

        let live = liveness_handler().await.into_response();
        let not_ready = readiness_handler(State(health.clone()))
            .await
            .into_response();
        assert_eq!(live.status(), StatusCode::OK);
        assert_eq!(not_ready.status(), StatusCode::SERVICE_UNAVAILABLE);

        mark_all_ready(&health);
        let ready = readiness_handler(State(health)).await.into_response();
        assert_eq!(ready.status(), StatusCode::OK);
    }
}
