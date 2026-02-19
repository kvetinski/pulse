use std::sync::Arc;
use std::time::Duration;

use pulse::application::scenarios::predefined_scenarios;
use pulse::application::service::{NodeRuntimeConfig, PulseNode, ScenarioExecutionPlan};
use pulse::domain::ports::{AccountGateway, PhoneGenerator};
use pulse::domain::scenario::StepPorts;
use pulse::infrastructure::config::AppConfig;
use pulse::infrastructure::grpc::account_gateway::TonicAccountGateway;
use pulse::infrastructure::kafka::{
    KafkaJobConsumer, KafkaJobPublisher, KafkaResultPublisher, ensure_topics,
};
use pulse::infrastructure::random::RandomUsPhoneGenerator;
use pulse::infrastructure::redis::{RedisDueStateStore, RedisIdempotencyStore, RedisLeaderElector};
use redis::Client;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_logging();

    let app_config = AppConfig::from_env();
    let scenarios = predefined_scenarios();
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

    let job_publisher =
        match KafkaJobPublisher::new(&app_config.kafka_brokers, &app_config.kafka_jobs_topic) {
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
        ],
    )
    .await;

    let result_publisher =
        match KafkaResultPublisher::new(&app_config.kafka_brokers, &app_config.kafka_results_topic)
        {
            Ok(p) => Arc::new(p),
            Err(err) => {
                error!(error = %err, "failed to create kafka result publisher");
                std::process::exit(1);
            }
        };

    let job_consumer = match KafkaJobConsumer::new(
        &app_config.kafka_brokers,
        &app_config.kafka_group_id,
        &app_config.kafka_jobs_topic,
    ) {
        Ok(c) => Arc::new(c),
        Err(err) => {
            error!(error = %err, "failed to create kafka job consumer");
            std::process::exit(1);
        }
    };

    let shared_phone_generator = Arc::new(RandomUsPhoneGenerator) as Arc<dyn PhoneGenerator>;
    let mut plans = Vec::with_capacity(scenarios.len());

    for scenario in scenarios {
        info!(
            scenario = %scenario.name,
            endpoint = %scenario.config.endpoint,
            scenarios_per_sec = scenario.config.scenarios_per_sec,
            duration_secs = scenario.config.duration.as_secs_f64(),
            "registering scenario"
        );
        let endpoint = scenario.config.endpoint.clone();
        let phone_generator = shared_phone_generator.clone();

        let account_gateway = match TonicAccountGateway::connect(&endpoint).await {
            Ok(client) => Arc::new(client) as Arc<dyn AccountGateway>,
            Err(err) => {
                error!(
                    scenario = %scenario.name,
                    endpoint = %endpoint,
                    error = %err,
                    "failed to initialize gRPC client"
                );
                continue;
            }
        };

        let ports = StepPorts {
            account_gateway,
            phone_generator,
        };
        plans.push(ScenarioExecutionPlan { scenario, ports });
    }

    let node = PulseNode::new(
        elector,
        due_store,
        job_publisher,
        job_consumer,
        idempotency_store,
        result_publisher,
        plans,
        NodeRuntimeConfig {
            leader_renew_interval: app_config.leader_renew_interval,
            scheduler_tick_interval: app_config.scheduler_tick_interval,
        },
    );
    node.run().await;
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
