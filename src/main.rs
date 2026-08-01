use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use pulse::application::aggregation_service::{AggregationRuntimeConfig, RunAggregationRuntime};
use pulse::application::scenarios::{
    MAX_CONFIGURED_SCENARIO_NAME_BYTES, MAX_CONFIGURED_SCENARIOS,
    MAX_CONFIGURED_STEPS_PER_SCENARIO, load_scenarios,
};
use pulse::application::service::{
    NodeRuntimeConfig, PulseNode, PulseNodeDependencies, ScenarioExecutionPlan,
    execution_semantics_fingerprint, planned_slice_loads, validate_scenario_identity_budget,
};
use pulse::domain::ports::DynamicGrpcGateway;
use pulse::domain::scenario::{Scenario, StepPorts};
use pulse::infrastructure::config::AppConfig;
use pulse::infrastructure::grpc::dynamic_gateway::DescriptorBackedGrpcGateway;
use pulse::infrastructure::kafka::{
    KafkaConsumerConfig, KafkaDlqPublisher, KafkaJobConsumer, KafkaJobPublisher,
    KafkaProducerConfig, KafkaResultConsumer, KafkaResultPublisher, KafkaSummaryPublisher,
    ensure_topics, probe_brokers, probe_required_topics,
};
use pulse::infrastructure::metrics::{
    HealthState, observe_shutdown_drain, spawn_metrics_server_with_health,
};
use pulse::infrastructure::redis::{
    RedisDueStateStore, RedisIdempotencyStore, RedisLeaderElector, RedisRunAggregationStore,
};
use redis::Client;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_logging();
    if let Err(error) = run().await {
        error!(error = %error, "Pulse stopped with an error");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = AppConfig::from_env().map_err(|error| error.to_string())?;
    let health = HealthState::new();
    health.set_config_loaded(true);

    let metrics_server = if !config.dry_run && config.metrics_enabled {
        Some(spawn_metrics_server_with_health(
            config.metrics_bind.clone(),
            health.clone(),
        ))
    } else {
        if !config.dry_run {
            warn!("metrics and health server explicitly disabled");
        }
        None
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let signal_health = health.clone();
    let signal_shutdown_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        await_shutdown_signal().await;
        signal_health.begin_draining();
        let _ = signal_shutdown_tx.send(true);
    });

    let scenarios = load_scenarios(&config)?;
    validate_scenario_limits(&config, &scenarios)?;
    info!(
        scenario_count = scenarios.len(),
        "loaded and validated scenarios"
    );

    // Descriptor and method validation is deliberately performed before any
    // Kafka/Redis startup work. A malformed plan is a configuration error, not
    // a partially healthy runtime.
    let offline_plans =
        build_scenario_plans(&config, scenarios.clone(), false, shutdown_rx.clone()).await?;
    if config.dry_run {
        print_dry_run(&offline_plans, config.startup_burst);
        return Ok(());
    }

    let startup = initialize_runtime(&config, scenarios, &health, shutdown_rx.clone());
    let runtime = tokio::select! {
        result = timeout(config.startup_deadline, startup) => {
            result.map_err(|_| format!(
                "startup deadline of {} ms expired before all required dependencies became ready",
                config.startup_deadline.as_millis()
            ))??
        }
        _ = wait_for_shutdown(shutdown_rx.clone()) => {
            return Err("shutdown requested during startup".to_string());
        }
    };

    health.set_worker_accepting(true);
    info!(
        guarantee =
            "at-least-once target execution; durable terminal publication before source commit",
        "Pulse runtime ready"
    );

    let monitor = tokio::spawn(dependency_health_loop(
        runtime.redis_client.clone(),
        config.kafka_brokers.clone(),
        required_kafka_topics(&config),
        config.kafka_request_timeout,
        health.clone(),
        shutdown_rx.clone(),
    ));

    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: runtime.elector.clone(),
            due_store: runtime.dispatch_store.clone(),
            job_publisher: runtime.job_publisher.clone(),
            job_consumer: runtime.job_consumer.clone(),
            idempotency_store: runtime.execution_store.clone(),
            result_publisher: runtime.result_publisher.clone(),
            dlq_publisher: runtime.dlq_publisher.clone(),
        },
        runtime.plans.clone(),
        NodeRuntimeConfig {
            leader_renew_interval: config.leader_renew_interval,
            scheduler_tick_interval: config.scheduler_tick_interval,
            worker_max_retries: config.worker_max_retries,
            worker_retry_base_delay: config.worker_retry_base_delay,
            worker_retry_max_delay: config.worker_retry_max_delay,
            worker_queue_capacity: config.retry_queue_capacity,
            execution_renew_interval: config.execution_lease_renew_interval,
            shutdown_drain_timeout: config.shutdown_drain_timeout,
            max_processing_interval: config.kafka_safe_processing_interval(),
            max_job_duration: config.max_duration,
            max_scenarios_per_sec: config.max_scenarios_per_sec,
            max_concurrency: config.max_concurrency,
            scenario_timeout: Some(config.grpc_scenario_timeout),
            startup_burst: config.startup_burst,
        },
    );

    let mut components = tokio::task::JoinSet::new();
    let node_shutdown = shutdown_rx.clone();
    components.spawn(async move {
        node.run(node_shutdown).await;
        Ok::<_, String>("scheduler/worker runtime")
    });
    if let Some(aggregation) = runtime.aggregation {
        let aggregation_config = AggregationRuntimeConfig {
            scan_interval: config.aggregation_scan_interval,
            scan_batch_limit: config.aggregation_scan_batch,
            outbox_batch_limit: config.aggregation_scan_batch,
            shutdown_drain_timeout: config.shutdown_drain_timeout,
            max_processing_interval: config.kafka_safe_processing_interval(),
        };
        let aggregator = RunAggregationRuntime::new(
            aggregation.store,
            aggregation.consumer,
            aggregation.publisher,
            runtime.dlq_publisher.clone(),
            aggregation_config,
        )?;
        let aggregation_shutdown = shutdown_rx.clone();
        components.spawn(async move {
            aggregator.run(aggregation_shutdown).await?;
            Ok::<_, String>("result aggregation runtime")
        });
    }

    let mut unexpected_exit = None;
    if let Some(mut server) = metrics_server {
        tokio::select! {
            _ = wait_for_shutdown(shutdown_rx.clone()) => {}
            component = components.join_next() => {
                unexpected_exit = component_exit(component, &shutdown_rx);
            }
            result = &mut server => {
                unexpected_exit = Some(format!("metrics/health server exited while the runtime was active: {result:?}"));
            }
        }
        health.begin_draining();
        let _ = shutdown_tx.send(true);
        drain_components(&mut components, runtime_join_timeout(&config)).await;
        server.abort();
    } else {
        tokio::select! {
            _ = wait_for_shutdown(shutdown_rx.clone()) => {}
            component = components.join_next() => {
                unexpected_exit = component_exit(component, &shutdown_rx);
            }
        }
        health.begin_draining();
        let _ = shutdown_tx.send(true);
        drain_components(&mut components, runtime_join_timeout(&config)).await;
    }
    health.begin_draining();
    monitor.abort();
    if let Some(error) = unexpected_exit {
        Err(error)
    } else {
        Ok(())
    }
}

fn component_exit(
    component: Option<Result<Result<&'static str, String>, tokio::task::JoinError>>,
    shutdown: &watch::Receiver<bool>,
) -> Option<String> {
    if *shutdown.borrow() {
        return None;
    }
    match component {
        Some(Ok(Ok(name))) => Some(format!("{name} exited unexpectedly")),
        Some(Ok(Err(error))) => Some(error),
        Some(Err(error)) => Some(format!(
            "runtime component panicked or was cancelled: {error}"
        )),
        None => Some("all runtime components exited unexpectedly".to_string()),
    }
}

async fn drain_components(
    components: &mut tokio::task::JoinSet<Result<&'static str, String>>,
    drain_timeout: Duration,
) {
    let started = std::time::Instant::now();
    let drain = async {
        while let Some(result) = components.join_next().await {
            match result {
                Ok(Ok(name)) => info!(component = name, "runtime component drained"),
                Ok(Err(error)) => warn!(error = %error, "runtime component failed while draining"),
                Err(error) => warn!(error = %error, "runtime component join failed while draining"),
            }
        }
    };
    if timeout(drain_timeout, drain).await.is_err() {
        warn!(
            drain_timeout_ms = drain_timeout.as_millis(),
            "runtime drain deadline expired; remaining work will be redelivered"
        );
        components.abort_all();
    }
    observe_shutdown_drain(started.elapsed());
}

fn runtime_join_timeout(config: &AppConfig) -> Duration {
    // Component runtimes consume PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS while
    // settling bounded work. Reserve additional time for the scheduler-stop
    // handshake, owner-checked leader relinquish, and final broker/Redis
    // request so the outer JoinSet deadline cannot abort cleanup at the exact
    // inner boundary.
    config
        .shutdown_drain_timeout
        .saturating_add(config.leader_renew_interval.saturating_mul(2))
        .saturating_add(config.kafka_request_timeout)
        .saturating_add(Duration::from_secs(1))
}

struct InitializedRuntime {
    redis_client: Client,
    elector: Arc<RedisLeaderElector>,
    dispatch_store: Arc<RedisDueStateStore>,
    execution_store: Arc<RedisIdempotencyStore>,
    job_publisher: Arc<KafkaJobPublisher>,
    result_publisher: Arc<KafkaResultPublisher>,
    dlq_publisher: Arc<KafkaDlqPublisher>,
    job_consumer: Arc<KafkaJobConsumer>,
    aggregation: Option<InitializedAggregation>,
    plans: Vec<ScenarioExecutionPlan>,
}

struct InitializedAggregation {
    store: Arc<RedisRunAggregationStore>,
    consumer: Arc<KafkaResultConsumer>,
    publisher: Arc<KafkaSummaryPublisher>,
}

async fn initialize_runtime(
    config: &AppConfig,
    scenarios: Vec<Scenario>,
    health: &HealthState,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<InitializedRuntime, String> {
    let redis_client = Client::open(config.redis_url.clone())
        .map_err(|error| format!("failed to create Redis client: {error}"))?;

    let redis_wait = wait_for_redis(redis_client.clone(), shutdown_rx.clone());
    let kafka_wait = wait_for_kafka(
        &config.kafka_brokers,
        config.kafka_request_timeout,
        shutdown_rx.clone(),
    );
    tokio::try_join!(redis_wait, kafka_wait)?;
    health.set_redis_ready(true);

    if config.kafka_topic_management_enabled {
        ensure_topics_with_retry(config, shutdown_rx.clone()).await?;
    } else {
        info!("Kafka topic management is disabled; expecting pre-provisioned topics");
    }
    wait_for_required_kafka_topics(
        &config.kafka_brokers,
        &required_kafka_topics(config),
        config.kafka_request_timeout,
        shutdown_rx.clone(),
    )
    .await?;
    health.set_kafka_topics_ready(true);

    let producer_config = KafkaProducerConfig {
        queue_capacity_messages: config.producer_queue_messages,
        message_max_bytes: config.kafka_producer_message_max_bytes,
        message_timeout: config.kafka_message_timeout,
        delivery_timeout: config.kafka_delivery_timeout,
        request_timeout: config.kafka_request_timeout,
        acks: config.kafka_producer_acks.clone(),
        enable_idempotence: config.kafka_producer_idempotence,
    };
    let consumer_config = KafkaConsumerConfig {
        max_poll_interval: config.kafka_max_poll_interval,
        session_timeout: config.kafka_session_timeout,
        request_timeout: config.kafka_request_timeout,
        prefetch_kib: config.consumer_queue_kbytes,
        partition_fetch_max_bytes: config.consumer_partition_fetch_max_bytes,
        fetch_max_bytes: config.consumer_fetch_max_bytes,
        record_max_bytes: config.consumer_record_max_bytes,
    };

    let job_publisher = Arc::new(KafkaJobPublisher::new_with_config(
        &config.kafka_brokers,
        &config.kafka_jobs_topic,
        producer_config.clone(),
    )?);
    let result_publisher = Arc::new(KafkaResultPublisher::new_with_config(
        &config.kafka_brokers,
        &config.kafka_results_topic,
        producer_config.clone(),
    )?);
    let dlq_publisher = Arc::new(KafkaDlqPublisher::new_with_config(
        &config.kafka_brokers,
        &config.kafka_dlq_topic,
        producer_config.clone(),
    )?);
    let summary_publisher = config
        .aggregation_enabled
        .then(|| {
            KafkaSummaryPublisher::new_with_config(
                &config.kafka_brokers,
                &config.kafka_summaries_topic,
                producer_config,
            )
            .map(Arc::new)
        })
        .transpose()?;
    health.set_kafka_producers_ready(true);

    let job_consumer = Arc::new(KafkaJobConsumer::new_with_config(
        &config.kafka_brokers,
        &config.kafka_group_id,
        &config.kafka_jobs_topic,
        consumer_config.clone(),
    )?);
    let result_consumer = config
        .aggregation_enabled
        .then(|| {
            KafkaResultConsumer::new_with_config(
                &config.kafka_brokers,
                &config.kafka_aggregator_group_id,
                &config.kafka_results_topic,
                consumer_config,
            )
            .map(Arc::new)
        })
        .transpose()?;
    health.set_kafka_consumer_ready(true);

    let plans = build_scenario_plans(config, scenarios, true, shutdown_rx).await?;
    health.set_scenarios_initialized(true);

    let redis_operation_timeout = config.redis_operation_timeout();
    let elector = Arc::new(
        RedisLeaderElector::new(
            redis_client.clone(),
            config.redis_leader_key.clone(),
            config.node_id.clone(),
            config.leader_lock_ttl_ms,
        )
        .with_operation_timeout(redis_operation_timeout),
    );
    let mut dispatch_store =
        RedisDueStateStore::new(redis_client.clone(), config.redis_schedule_prefix.clone())
            .with_operation_timeout(redis_operation_timeout);
    if config.aggregation_enabled {
        dispatch_store = dispatch_store.with_aggregation_registration(
            config.redis_aggregation_prefix.clone(),
            config.aggregation_partial_timeout,
            config.aggregation_retention,
            config.aggregation_max_active_runs,
        );
    }
    let dispatch_store = Arc::new(dispatch_store);
    let execution_store = Arc::new(RedisIdempotencyStore::with_timings(
        redis_client.clone(),
        config.redis_idempotency_prefix.clone(),
        config.execution_lease_ttl,
        config.execution_terminal_retention,
        redis_operation_timeout,
    ));
    let aggregation = match (result_consumer, summary_publisher) {
        (Some(consumer), Some(publisher)) => {
            let store = RedisRunAggregationStore::new(
                redis_client.clone(),
                config.redis_aggregation_prefix.clone(),
                config.aggregation_partial_timeout,
                config.aggregation_retention,
            )
            .and_then(|store| store.with_operation_timeout(redis_operation_timeout))
            .and_then(|store| store.with_max_error_kinds(config.aggregation_max_error_kinds))
            .and_then(|store| store.with_max_active_runs(config.aggregation_max_active_runs))
            .and_then(|store| store.with_max_scan_limit(config.aggregation_scan_batch))
            .map_err(|error| error.to_string())?;
            Some(InitializedAggregation {
                store: Arc::new(store),
                consumer,
                publisher,
            })
        }
        (None, None) => {
            warn!("distributed run aggregation explicitly disabled");
            None
        }
        _ => return Err("aggregation dependencies initialized inconsistently".to_string()),
    };

    Ok(InitializedRuntime {
        redis_client,
        elector,
        dispatch_store,
        execution_store,
        job_publisher,
        result_publisher,
        dlq_publisher,
        job_consumer,
        aggregation,
        plans,
    })
}

async fn build_scenario_plans(
    config: &AppConfig,
    scenarios: Vec<Scenario>,
    connect_targets: bool,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<Vec<ScenarioExecutionPlan>, String> {
    let mut plans = Vec::with_capacity(scenarios.len());
    let mut clients: HashMap<String, Arc<dyn DynamicGrpcGateway>> = HashMap::new();
    let mut failures = Vec::new();
    let descriptor_bytes = config
        .grpc_descriptor_set
        .as_deref()
        .map(|path| {
            std::fs::read(path)
                .map_err(|error| format!("could not read gRPC descriptor set '{path}': {error}"))
        })
        .transpose()?;
    let execution_semantics_fingerprint = execution_semantics_fingerprint(
        config.grpc_request_timeout,
        Some(config.grpc_scenario_timeout),
        descriptor_bytes.as_deref(),
    );

    for scenario in scenarios {
        let mut required_endpoints = HashSet::new();
        for step in &scenario.steps {
            if step.requires_dynamic_grpc() {
                required_endpoints.insert(
                    step.dynamic_grpc_endpoint_override()
                        .unwrap_or(&scenario.config.endpoint)
                        .to_string(),
                );
            }
        }

        let result: Result<ScenarioExecutionPlan, String> = async {
            let mut scenario_clients = HashMap::new();
            if !required_endpoints.is_empty() {
                let descriptor_path = config.grpc_descriptor_set.as_deref().ok_or_else(|| {
                    format!(
                        "scenario '{}' uses dynamic gRPC but PULSE_GRPC_DESCRIPTOR_SET is unset",
                        scenario.name
                    )
                })?;

                for endpoint in required_endpoints {
                    let client = if let Some(existing) = clients.get(&endpoint) {
                        existing.clone()
                    } else {
                        let gateway = if connect_targets {
                            connect_gateway_with_retry(
                                &endpoint,
                                descriptor_path,
                                config.grpc_connect_timeout,
                                config.grpc_request_timeout,
                                shutdown_rx.clone(),
                            )
                            .await?
                        } else {
                            DescriptorBackedGrpcGateway::from_descriptor_set(
                                &endpoint,
                                descriptor_path,
                                config.grpc_request_timeout,
                            )
                            .map_err(|error| error.to_string())?
                        };
                        let gateway = Arc::new(gateway) as Arc<dyn DynamicGrpcGateway>;
                        clients.insert(endpoint.clone(), gateway.clone());
                        gateway
                    };
                    scenario_clients.insert(endpoint, client);
                }
            }

            let ports = StepPorts {
                default_endpoint: scenario.config.endpoint.clone(),
                dynamic_grpc_gateways: scenario_clients,
            };
            for step in &scenario.steps {
                step.validate(&ports).map_err(|error| {
                    format!(
                        "scenario '{}' step '{}' failed startup validation: {error}",
                        scenario.name,
                        step.name()
                    )
                })?;
            }
            Ok(ScenarioExecutionPlan {
                scenario,
                ports,
                execution_semantics_fingerprint: execution_semantics_fingerprint.clone(),
            })
        }
        .await;

        match result {
            Ok(plan) => plans.push(plan),
            Err(error) if config.allow_partial_start => {
                warn!(error = %error, "scenario excluded because partial start is enabled");
                failures.push(error);
            }
            Err(error) => return Err(error),
        }
    }

    if plans.is_empty() {
        return Err(format!(
            "no valid scenarios remain after initialization{}",
            if failures.is_empty() {
                String::new()
            } else {
                format!(": {}", failures.join("; "))
            }
        ));
    }
    Ok(plans)
}

fn validate_scenario_limits(config: &AppConfig, scenarios: &[Scenario]) -> Result<(), String> {
    if scenarios.is_empty() {
        return Err("scenario source contains zero scenarios".to_string());
    }
    if scenarios.len() > MAX_CONFIGURED_SCENARIOS {
        return Err(format!(
            "scenario source contains {} scenarios; maximum is {MAX_CONFIGURED_SCENARIOS}",
            scenarios.len()
        ));
    }
    for scenario in scenarios {
        if scenario.name.trim().is_empty()
            || scenario.name.len() > MAX_CONFIGURED_SCENARIO_NAME_BYTES
        {
            return Err(format!(
                "scenario name must be non-empty and at most {MAX_CONFIGURED_SCENARIO_NAME_BYTES} bytes"
            ));
        }
        if scenario.steps.is_empty() || scenario.steps.len() > MAX_CONFIGURED_STEPS_PER_SCENARIO {
            return Err(format!(
                "scenario '{}' must contain between 1 and {MAX_CONFIGURED_STEPS_PER_SCENARIO} steps",
                scenario.name
            ));
        }
        validate_scenario_identity_budget(&scenario.name)
            .map_err(|error| format!("scenario '{}': {error}", scenario.name))?;
        if let pulse::domain::scenario::RepeatPolicy::Every(interval) = &scenario.config.repeat
            && *interval < scenario.config.duration
        {
            return Err(format!(
                "scenario '{}' repeat interval {} ms is shorter than its {} ms load window and would overlap global concurrency budgets",
                scenario.name,
                interval.as_millis(),
                scenario.config.duration.as_millis()
            ));
        }
        if scenario.config.duration > config.max_duration {
            return Err(format!(
                "scenario '{}' duration {} ms exceeds PULSE_MAX_DURATION_MS={}",
                scenario.name,
                scenario.config.duration.as_millis(),
                config.max_duration.as_millis()
            ));
        }
        if config.startup_burst == 0
            && scenario.config.scenarios_per_sec * scenario.config.duration.as_secs_f64() < 1.0
        {
            return Err(format!(
                "scenario '{}' window is shorter than one {:.6} SPS inter-arrival interval; increase duration or explicitly opt into PULSE_STARTUP_BURST",
                scenario.name, scenario.config.scenarios_per_sec
            ));
        }
        if !scenario.config.scenarios_per_sec.is_finite()
            || scenario.config.scenarios_per_sec <= 0.0
            || scenario.config.scenarios_per_sec > config.max_scenarios_per_sec
        {
            return Err(format!(
                "scenario '{}' rate {} is outside (0, PULSE_MAX_SCENARIOS_PER_SEC={}]",
                scenario.name, scenario.config.scenarios_per_sec, config.max_scenarios_per_sec
            ));
        }
        if scenario.config.max_concurrency == 0
            || scenario.config.max_concurrency > config.max_concurrency
        {
            return Err(format!(
                "scenario '{}' concurrency {} is outside [1, PULSE_MAX_CONCURRENCY={}]",
                scenario.name, scenario.config.max_concurrency, config.max_concurrency
            ));
        }
        if config.startup_burst > scenario.config.max_concurrency {
            return Err(format!(
                "scenario '{}' startup burst {} exceeds its max concurrency {}",
                scenario.name, config.startup_burst, scenario.config.max_concurrency
            ));
        }
        config
            .validate_target_endpoint(&scenario.config.endpoint)
            .map_err(|error| format!("scenario '{}': {error}", scenario.name))?;
        for step in &scenario.steps {
            if let Some(endpoint) = step.dynamic_grpc_endpoint_override() {
                config.validate_target_endpoint(endpoint).map_err(|error| {
                    format!(
                        "scenario '{}' step '{}': {error}",
                        scenario.name,
                        step.name()
                    )
                })?;
            }
        }
    }
    Ok(())
}

fn print_dry_run(plans: &[ScenarioExecutionPlan], global_startup_burst: usize) {
    println!("Pulse dry run (no target traffic, Kafka, or Redis operations):");
    for plan in plans {
        let loads = planned_slice_loads(&plan.scenario, global_startup_burst);
        println!(
            "scenario={} endpoint={} duration_ms={} repeat={:?} slices={}",
            plan.scenario.name,
            plan.scenario.config.endpoint,
            plan.scenario.config.duration.as_millis(),
            plan.scenario.config.repeat,
            loads.len()
        );
        for (index, load) in loads.iter().enumerate() {
            println!(
                "  slice={}/{} scenarios_per_sec={:.12} max_concurrency={} startup_burst={} duration_ms={}",
                index,
                loads.len(),
                load.scenarios_per_sec,
                load.max_concurrency,
                load.startup_burst,
                load.duration.as_millis()
            );
        }
    }
}

async fn connect_gateway_with_retry(
    endpoint: &str,
    descriptor_path: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<DescriptorBackedGrpcGateway, String> {
    let budget = connect_timeout
        .saturating_mul(3)
        .max(Duration::from_secs(1));
    let endpoint = endpoint.to_string();
    let timeout_endpoint = endpoint.clone();
    let descriptor_path = descriptor_path.to_string();
    timeout(budget, async move {
        let mut shutdown_rx = shutdown_rx;
        let mut attempt = 0_u32;
        loop {
            attempt = attempt.saturating_add(1);
            match DescriptorBackedGrpcGateway::connect_with_timeouts(
                &endpoint,
                &descriptor_path,
                connect_timeout,
                request_timeout,
            )
            .await
            {
                Ok(gateway) => return Ok(gateway),
                Err(error) => {
                    warn!(%endpoint, attempt, error = %error, "gRPC target initialization failed; retrying within startup deadline");
                    wait_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await?;
                }
            }
        }
    })
    .await
    .map_err(|_| format!("gRPC target '{timeout_endpoint}' did not become ready within {} ms", budget.as_millis()))?
}

async fn wait_for_redis(
    client: Client,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        let probe = async {
            let mut connection = client
                .get_multiplexed_tokio_connection()
                .await
                .map_err(|error| error.to_string())?;
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
                .map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        };
        match timeout(Duration::from_secs(2), probe).await {
            Ok(Ok(())) => {
                info!(attempt, "Redis is ready");
                return Ok(());
            }
            Ok(Err(error)) => warn!(attempt, error = %error, "Redis readiness probe failed"),
            Err(_) => warn!(attempt, "Redis readiness probe timed out"),
        }
        wait_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await?;
    }
}

async fn wait_for_kafka(
    brokers: &str,
    probe_timeout: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        match probe_brokers(brokers, probe_timeout).await {
            Ok(()) => {
                info!(attempt, "Kafka brokers are ready");
                return Ok(());
            }
            Err(error) => warn!(attempt, error = %error, "Kafka readiness probe failed"),
        }
        wait_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await?;
    }
}

async fn ensure_topics_with_retry(
    config: &AppConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let topics = [
        (
            config.kafka_jobs_topic.as_str(),
            config.kafka_topic_partitions,
            config.kafka_topic_replication_factor,
        ),
        (
            config.kafka_results_topic.as_str(),
            config.kafka_topic_partitions,
            config.kafka_topic_replication_factor,
        ),
        (
            config.kafka_summaries_topic.as_str(),
            config.kafka_topic_partitions,
            config.kafka_topic_replication_factor,
        ),
        (
            config.kafka_dlq_topic.as_str(),
            config.kafka_topic_partitions,
            config.kafka_topic_replication_factor,
        ),
    ];
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        match ensure_topics(&config.kafka_brokers, &topics).await {
            Ok(()) => {
                info!(attempt, "Kafka topics are ready");
                return Ok(());
            }
            Err(error) => warn!(attempt, error = %error, "Kafka topic creation failed; retrying"),
        }
        wait_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await?;
    }
}

fn required_kafka_topics(config: &AppConfig) -> Vec<String> {
    vec![
        config.kafka_jobs_topic.clone(),
        config.kafka_results_topic.clone(),
        config.kafka_summaries_topic.clone(),
        config.kafka_dlq_topic.clone(),
    ]
}

async fn wait_for_required_kafka_topics(
    brokers: &str,
    required_topics: &[String],
    probe_timeout: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        match probe_required_topics(brokers, required_topics, probe_timeout).await {
            Ok(()) => {
                info!(attempt, "required Kafka topics and partitions are ready");
                return Ok(());
            }
            Err(error) => {
                warn!(attempt, error = %error, "required Kafka topic readiness probe failed")
            }
        }
        wait_or_shutdown(Duration::from_millis(500), &mut shutdown_rx).await?;
    }
}

async fn dependency_health_loop(
    redis_client: Client,
    kafka_brokers: String,
    required_kafka_topics: Vec<String>,
    probe_timeout: Duration,
    health: HealthState,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    loop {
        if wait_or_shutdown(Duration::from_secs(2), &mut shutdown_rx)
            .await
            .is_err()
        {
            return;
        }

        let redis_ready = timeout(Duration::from_secs(2), async {
            let mut connection = redis_client.get_multiplexed_tokio_connection().await?;
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
        })
        .await
        .is_ok_and(|result| result.is_ok());
        health.set_redis_ready(redis_ready);

        let kafka_ready =
            probe_required_topics(&kafka_brokers, &required_kafka_topics, probe_timeout)
                .await
                .is_ok();
        health.set_kafka_topics_ready(kafka_ready);
    }
}

async fn wait_or_shutdown(
    delay: Duration,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    if *shutdown_rx.borrow() {
        return Err("shutdown requested".to_string());
    }
    tokio::select! {
        _ = sleep(delay) => Ok(()),
        changed = shutdown_rx.changed() => {
            match changed {
                Ok(()) if *shutdown_rx.borrow() => Err("shutdown requested".to_string()),
                Ok(()) => Ok(()),
                Err(_) => Err("shutdown signal channel closed".to_string()),
            }
        }
    }
}

async fn wait_for_shutdown(mut shutdown_rx: watch::Receiver<bool>) {
    if *shutdown_rx.borrow() {
        return;
    }
    while shutdown_rx.changed().await.is_ok() {
        if *shutdown_rx.borrow() {
            return;
        }
    }
}

fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();
}

async fn await_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl+C, starting graceful shutdown"),
        _ = terminate => info!("received SIGTERM, starting graceful shutdown"),
    }
}
