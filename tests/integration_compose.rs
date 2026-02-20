use std::time::Duration;

use pulse::application::service::{
    CommitableJob, DueStateStore, IdempotencyStore, JobConsumer, JobPublisher,
};
use pulse::domain::contracts::{JobLoadConfig, JobSlice, ScenarioJob};
use pulse::domain::scenario::RepeatPolicy;
use pulse::infrastructure::kafka::{KafkaJobConsumer, KafkaJobPublisher, ensure_topics};
use pulse::infrastructure::redis::{RedisDueStateStore, RedisIdempotencyStore};
use redis::Client;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

fn test_kafka_brokers() -> String {
    std::env::var("PULSE_TEST_KAFKA_BROKERS").unwrap_or_else(|_| "127.0.0.1:19092".to_string())
}

fn test_redis_url() -> String {
    std::env::var("PULSE_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string())
}

fn sample_job(scenario_id: &str, execution_key: String) -> ScenarioJob {
    ScenarioJob {
        schema_version: 1,
        scenario_id: scenario_id.to_string(),
        run_id: Uuid::new_v4().to_string(),
        execution_key,
        scheduled_at_unix_ms: 0,
        slice: JobSlice { index: 0, total: 1 },
        load: JobLoadConfig {
            scenarios_per_sec: 1.0,
            duration: Duration::from_secs(1),
            max_concurrency: 1,
        },
        attempt: 0,
        max_retries: 0,
    }
}

#[tokio::test]
#[ignore = "requires docker compose dependencies (kafka + redis)"]
async fn kafka_job_roundtrip_via_compose() {
    let brokers = test_kafka_brokers();
    let topic = format!("pulse.test.jobs.{}", Uuid::new_v4().simple());
    let group_id = format!("pulse-test-group-{}", Uuid::new_v4().simple());

    ensure_topics(&brokers, &[(&topic, 1, 1)])
        .await
        .expect("failed to ensure test topic");

    let publisher = KafkaJobPublisher::new(&brokers, &topic, 1024)
        .expect("failed to create kafka job publisher");
    let consumer = KafkaJobConsumer::new(&brokers, &group_id, &topic, 1024)
        .expect("failed to create kafka job consumer");

    let execution_key = format!("kafka-roundtrip-{}", Uuid::new_v4());
    let job = sample_job("ComposeKafkaRoundtrip", execution_key.clone());
    publisher
        .publish_job("partition-key", &job)
        .await
        .expect("failed to publish kafka job");

    let consumed = timeout(Duration::from_secs(15), async {
        loop {
            if let Some(message) = consumer.recv().await.expect("consumer recv failed") {
                break message;
            }
        }
    })
    .await
    .expect("timed out waiting for kafka message");

    assert_eq!(consumed.job.execution_key, execution_key);
    assert_eq!(consumed.job.scenario_id, "ComposeKafkaRoundtrip");
    consumed.commit().expect("failed to commit consumed job");
}

#[tokio::test]
#[ignore = "requires docker compose dependencies (kafka + redis)"]
async fn redis_due_and_idempotency_via_compose() {
    let client = Client::open(test_redis_url()).expect("failed to create redis client");

    let schedule_prefix = format!("pulse:test:schedule:{}", Uuid::new_v4().simple());
    let due_store = RedisDueStateStore::new(client.clone(), schedule_prefix);

    assert!(
        due_store
            .claim_due("scenario-once", RepeatPolicy::Once)
            .await
    );
    assert!(
        !due_store
            .claim_due("scenario-once", RepeatPolicy::Once)
            .await
    );

    let interval = Duration::from_millis(150);
    assert!(
        due_store
            .claim_due("scenario-every", RepeatPolicy::Every(interval))
            .await
    );
    assert!(
        !due_store
            .claim_due("scenario-every", RepeatPolicy::Every(interval))
            .await
    );
    sleep(Duration::from_millis(180)).await;
    assert!(
        due_store
            .claim_due("scenario-every", RepeatPolicy::Every(interval))
            .await
    );

    let dedupe_prefix = format!("pulse:test:dedupe:{}", Uuid::new_v4().simple());
    let idempotency = RedisIdempotencyStore::new(client, dedupe_prefix, Duration::from_secs(30));
    assert!(idempotency.claim_once("execution-1").await);
    assert!(!idempotency.claim_once("execution-1").await);
}
