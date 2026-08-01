use std::sync::Arc;
use std::time::Duration;

use pulse::application::service::{CommitableJob, JobConsumer, JobPublisher};
use pulse::domain::contracts::{CURRENT_CONTRACT_VERSION, JobLoadConfig, JobSlice, ScenarioJob};
use pulse::domain::coordination::{
    ClaimOutcome, ExecutionClaim, ExecutionLeaseStore, TerminalOutcome,
};
use pulse::infrastructure::kafka::{KafkaJobConsumer, KafkaJobPublisher, ensure_topics};
use pulse::infrastructure::redis::RedisIdempotencyStore;
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
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: scenario_id.to_string(),
        run_id: Uuid::new_v4().to_string(),
        execution_key,
        plan_fingerprint: "fnv128:compose-fixture".to_string(),
        scheduled_at_unix_ms: 0,
        not_before_unix_ms: 0,
        slice: JobSlice { index: 0, total: 1 },
        load: JobLoadConfig {
            scenarios_per_sec: 1.0,
            duration: Duration::from_secs(1),
            max_concurrency: 1,
            startup_burst: 0,
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

    let consumed_job = consumed.job().expect("valid consumed job");
    assert_eq!(consumed_job.execution_key, execution_key);
    assert_eq!(consumed_job.scenario_id, "ComposeKafkaRoundtrip");
    consumed
        .commit()
        .await
        .expect("failed to synchronously commit consumed job");
}

#[tokio::test]
#[ignore = "requires docker compose dependencies (kafka + redis)"]
async fn kafka_rebalance_fences_buffered_commit_and_redelivery_recovers() {
    let brokers = test_kafka_brokers();
    let topic = format!("pulse.test.rebalance.{}", Uuid::new_v4().simple());
    let group_id = format!("pulse-test-rebalance-{}", Uuid::new_v4().simple());

    ensure_topics(&brokers, &[(&topic, 1, 1)])
        .await
        .expect("failed to ensure rebalance topic");
    let publisher = KafkaJobPublisher::new(&brokers, &topic, 1024)
        .expect("failed to create rebalance publisher");
    let first = Arc::new(
        KafkaJobConsumer::new(&brokers, &group_id, &topic, 1024)
            .expect("failed to create first consumer"),
    );
    let execution_key = format!("kafka-rebalance-{}", Uuid::new_v4());
    publisher
        .publish_job(
            "rebalance-key",
            &sample_job("ComposeKafkaRebalance", execution_key.clone()),
        )
        .await
        .expect("failed to publish rebalance job");

    let buffered = timeout(Duration::from_secs(15), first.recv())
        .await
        .expect("timed out waiting for buffered job")
        .expect("first consumer receive failed")
        .expect("rebalance topic returned no record");
    let buffered_epoch = first.rebalance_epoch();
    let second = Arc::new(
        KafkaJobConsumer::new(&brokers, &group_id, &topic, 1024)
            .expect("failed to create second consumer"),
    );
    let second_poll = {
        let second = second.clone();
        tokio::spawn(async move { second.recv().await })
    };
    let first_poll = {
        let first = first.clone();
        tokio::spawn(async move { first.recv().await })
    };

    timeout(Duration::from_secs(15), async {
        while first.rebalance_epoch() == buffered_epoch {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("first consumer did not observe the group rebalance");

    let commit_error = buffered
        .commit()
        .await
        .expect_err("a buffered record from an old assignment epoch must not commit");
    assert!(commit_error.contains("rebalance"));
    first_poll.abort();
    let _ = first_poll.await;
    second_poll.abort();
    let _ = second_poll.await;
    drop(first);
    drop(second);

    let recovery = KafkaJobConsumer::new(&brokers, &group_id, &topic, 1024)
        .expect("failed to create recovery consumer");
    let redelivered = timeout(Duration::from_secs(20), recovery.recv())
        .await
        .expect("timed out waiting for rebalance redelivery")
        .expect("recovery consumer receive failed")
        .expect("recovery topic returned no record");
    assert_eq!(
        redelivered
            .job()
            .expect("redelivered job is valid")
            .execution_key,
        execution_key
    );
    redelivered
        .commit()
        .await
        .expect("recovered assignment commits synchronously");
}

#[tokio::test]
#[ignore = "requires docker compose dependencies (kafka + redis)"]
async fn redis_execution_lease_via_compose() {
    let client = Client::open(test_redis_url()).expect("failed to create redis client");
    let dedupe_prefix = format!("pulse:test:dedupe:{}", Uuid::new_v4().simple());
    let leases = RedisIdempotencyStore::with_timings(
        client,
        dedupe_prefix,
        Duration::from_secs(5),
        Duration::from_secs(30),
        Duration::from_secs(1),
    );
    let claim = ExecutionClaim::new("execution-1", 0);
    let lease = match leases.claim(&claim).await.expect("first claim succeeds") {
        ClaimOutcome::Acquired(lease) => lease,
        other => panic!("expected acquired lease, got {other:?}"),
    };
    assert!(matches!(
        leases.claim(&claim).await.expect("contention is typed"),
        ClaimOutcome::Busy { .. }
    ));
    leases
        .complete(&lease, TerminalOutcome::ResultPublished)
        .await
        .expect("owner completes the execution");
    assert!(matches!(
        leases.claim(&claim).await.expect("terminal state is durable"),
        ClaimOutcome::AlreadyCompleted(ref completed)
            if completed.outcome == TerminalOutcome::ResultPublished
    ));
}
