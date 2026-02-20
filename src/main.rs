use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use pulse::application::scenarios::load_scenarios;
use pulse::application::service::{
    NodeRuntimeConfig, PulseNode, PulseNodeDependencies, ScenarioExecutionPlan,
};
use pulse::domain::ports::DynamicGrpcGateway;
use pulse::domain::scenario::StepPorts;
use pulse::infrastructure::config::AppConfig;
use pulse::infrastructure::grpc::dynamic_gateway::DescriptorBackedGrpcGateway;
use pulse::infrastructure::kafka::{
    KafkaDlqPublisher, KafkaJobConsumer, KafkaJobPublisher, KafkaResultPublisher, ensure_topics,
};
use pulse::infrastructure::metrics::spawn_metrics_server;
use pulse::infrastructure::redis::{RedisDueStateStore, RedisIdempotencyStore, RedisLeaderElector};
use redis::Client;
use tokio::sync::watch;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_logging();

    let app_config = AppConfig::from_env();
    if app_config.metrics_enabled {
        spawn_metrics_server(app_config.metrics_bind.clone());
    }

    let scenarios = match load_scenarios(&app_config) {
        Ok(scenarios) => scenarios,
        Err(err) => {
            error!(error = %err, "failed to load scenarios");
            std::process::exit(1);
        }
    };
    info!(scenario_count = scenarios.len(), "loaded scenarios");

    let redis_client = match Client::open(app_config.redis_url.clone()) {
        Ok(client) => client,
        Err(err) => {
            error!(error = %err, "failed to create redis client");
            std::process::exit(1);
        }
    };

    let elector = Arc::new(RedisLeaderElector::new(
        redis_client.clone(),
        app_config.redis_leader_key.clone(),
        app_config.node_id.clone(),
        app_config.leader_lock_ttl_ms,
    ));
    let due_store = Arc::new(RedisDueStateStore::new(
        redis_client.clone(),
        app_config.redis_schedule_prefix.clone(),
    ));
    let idempotency_store = Arc::new(RedisIdempotencyStore::new(
        redis_client,
        app_config.redis_idempotency_prefix.clone(),
        Duration::from_secs(3600),
    ));

    let job_publisher = match KafkaJobPublisher::new(
        &app_config.kafka_brokers,
        &app_config.kafka_jobs_topic,
        app_config.queue_capacity,
    ) {
        Ok(p) => Arc::new(p),
        Err(err) => {
            error!(error = %err, "failed to create kafka job publisher");
            std::process::exit(1);
        }
    };

    ensure_topics_with_retry(
        &app_config.kafka_brokers,
        &[
            (&app_config.kafka_jobs_topic, 12, 1),
            (&app_config.kafka_results_topic, 12, 1),
            (&app_config.kafka_dlq_topic, 12, 1),
        ],
    )
    .await;

    let result_publisher = match KafkaResultPublisher::new(
        &app_config.kafka_brokers,
        &app_config.kafka_results_topic,
        app_config.queue_capacity,
    ) {
        Ok(p) => Arc::new(p),
        Err(err) => {
            error!(error = %err, "failed to create kafka result publisher");
            std::process::exit(1);
        }
    };

    let dlq_publisher = match KafkaDlqPublisher::new(
        &app_config.kafka_brokers,
        &app_config.kafka_dlq_topic,
        app_config.queue_capacity,
    ) {
        Ok(p) => Arc::new(p),
        Err(err) => {
            error!(error = %err, "failed to create kafka dead-letter publisher");
            std::process::exit(1);
        }
    };

    let job_consumer = match KafkaJobConsumer::new(
        &app_config.kafka_brokers,
        &app_config.kafka_group_id,
        &app_config.kafka_jobs_topic,
        app_config.queue_capacity,
    ) {
        Ok(c) => Arc::new(c),
        Err(err) => {
            error!(error = %err, "failed to create kafka job consumer");
            std::process::exit(1);
        }
    };

    let mut plans = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        let mut required_endpoints = HashSet::new();
        for step in &scenario.steps {
            if !step.requires_dynamic_grpc() {
                continue;
            }
            let endpoint = step
                .dynamic_grpc_endpoint_override()
                .unwrap_or(&scenario.config.endpoint);
            required_endpoints.insert(endpoint.to_string());
        }
        let requires_dynamic_grpc = !required_endpoints.is_empty();

        info!(
            scenario = %scenario.name,
            endpoint = %scenario.config.endpoint,
            scenarios_per_sec = scenario.config.scenarios_per_sec,
            duration_secs = scenario.config.duration.as_secs_f64(),
            requires_dynamic_grpc,
            "registering scenario"
        );
        let dynamic_grpc_gateways = if requires_dynamic_grpc {
            let Some(descriptor_set_path) = app_config.grpc_descriptor_set.as_deref() else {
                error!(
                    scenario = %scenario.name,
                    endpoint = %scenario.config.endpoint,
                    "scenario uses dynamic gRPC steps but PULSE_GRPC_DESCRIPTOR_SET is not set"
                );
                continue;
            };

            let mut clients: HashMap<String, Arc<dyn DynamicGrpcGateway>> = HashMap::new();
            let mut failed = false;
            for endpoint in &required_endpoints {
                match DescriptorBackedGrpcGateway::connect(endpoint, descriptor_set_path).await {
                    Ok(client) => {
                        clients.insert(
                            endpoint.clone(),
                            Arc::new(client) as Arc<dyn DynamicGrpcGateway>,
                        );
                    }
                    Err(err) => {
                        error!(
                            scenario = %scenario.name,
                            endpoint = %endpoint,
                            descriptor_set_path = %descriptor_set_path,
                            error = %err,
                            "failed to initialize dynamic gRPC client"
                        );
                        failed = true;
                        break;
                    }
                }
            }
            if failed {
                continue;
            }
            clients
        } else {
            HashMap::new()
        };

        let ports = StepPorts {
            default_endpoint: scenario.config.endpoint.clone(),
            dynamic_grpc_gateways,
        };
        plans.push(ScenarioExecutionPlan { scenario, ports });
    }

    let node = PulseNode::new(
        PulseNodeDependencies {
            elector,
            due_store,
            job_publisher,
            job_consumer,
            idempotency_store,
            result_publisher,
            dlq_publisher,
        },
        plans,
        NodeRuntimeConfig {
            leader_renew_interval: app_config.leader_renew_interval,
            scheduler_tick_interval: app_config.scheduler_tick_interval,
            worker_max_retries: app_config.worker_max_retries,
            worker_retry_base_delay: app_config.worker_retry_base_delay,
        },
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        await_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    node.run(shutdown_rx).await;
}

fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .init();
}

async fn ensure_topics_with_retry(brokers: &str, topics: &[(&str, i32, i32)]) {
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match ensure_topics(brokers, topics).await {
            Ok(_) => {
                info!(attempt, "kafka topics are ready");
                return;
            }
            Err(err) => {
                warn!(
                    attempt,
                    error = %err,
                    "failed to ensure kafka topics; retrying"
                );
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
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
        _ = ctrl_c => {
            info!("received Ctrl+C, starting graceful shutdown");
        }
        _ = terminate => {
            info!("received SIGTERM, starting graceful shutdown");
        }
    }
}
