use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use pulse::application::service::{
    CommitableJob, DlqPublisher, DueStateStore, IdempotencyStore, JobConsumer, JobPublisher,
    LeaderElector, NodeRuntimeConfig, PulseNode, PulseNodeDependencies, ResultPublisher,
    ScenarioExecutionPlan,
};
use pulse::domain::context::ScenarioContext;
use pulse::domain::contracts::{
    PartitionKeyStrategy, ScenarioJob, ScenarioRunResult, ScenarioRunStatus,
};
use pulse::domain::error::PulseError;
use pulse::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step, StepPorts};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::{sleep, timeout};

#[derive(Default)]
struct NoopLeaderElector;

#[async_trait]
impl LeaderElector for NoopLeaderElector {
    async fn try_acquire_or_renew(&self) -> bool {
        true
    }

    async fn relinquish(&self) {}
}

#[derive(Default)]
struct OnceDueStateStore {
    seen: Mutex<HashSet<String>>,
}

#[async_trait]
impl DueStateStore for OnceDueStateStore {
    async fn claim_due(&self, scenario_id: &str, _repeat: RepeatPolicy) -> bool {
        let mut seen = self.seen.lock().await;
        if seen.contains(scenario_id) {
            return false;
        }
        seen.insert(scenario_id.to_string());
        true
    }
}

#[derive(Default)]
struct InMemoryIdempotencyStore {
    claimed: Mutex<HashSet<String>>,
}

#[async_trait]
impl IdempotencyStore for InMemoryIdempotencyStore {
    async fn claim_once(&self, execution_key: &str) -> bool {
        let mut claimed = self.claimed.lock().await;
        claimed.insert(execution_key.to_string())
    }
}

#[derive(Clone)]
struct InMemoryJobPublisher {
    tx: mpsc::UnboundedSender<InMemoryJobMessage>,
    published_keys: Arc<Mutex<Vec<String>>>,
    duplicate_each_job: bool,
    commit_counter: Arc<AtomicUsize>,
}

impl InMemoryJobPublisher {
    fn new(
        tx: mpsc::UnboundedSender<InMemoryJobMessage>,
        duplicate_each_job: bool,
        commit_counter: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            tx,
            published_keys: Arc::new(Mutex::new(Vec::new())),
            duplicate_each_job,
            commit_counter,
        }
    }
}

#[async_trait]
impl JobPublisher for InMemoryJobPublisher {
    async fn publish_job(&self, key: &str, job: &ScenarioJob) -> Result<(), String> {
        self.published_keys.lock().await.push(key.to_string());

        let msg = InMemoryJobMessage {
            job: job.clone(),
            commit_counter: self.commit_counter.clone(),
        };
        self.tx
            .send(msg.clone())
            .map_err(|e| format!("failed to publish in-memory job: {e}"))?;

        if self.duplicate_each_job {
            self.tx
                .send(msg)
                .map_err(|e| format!("failed to publish duplicate in-memory job: {e}"))?;
        }

        Ok(())
    }
}

struct InMemoryJobConsumer {
    rx: Mutex<mpsc::UnboundedReceiver<InMemoryJobMessage>>,
}

impl InMemoryJobConsumer {
    fn new(rx: mpsc::UnboundedReceiver<InMemoryJobMessage>) -> Self {
        Self { rx: Mutex::new(rx) }
    }
}

#[async_trait]
impl JobConsumer for InMemoryJobConsumer {
    type Item = InMemoryJobMessage;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        let mut rx = self.rx.lock().await;
        Ok(rx.recv().await)
    }
}

#[derive(Clone)]
struct InMemoryJobMessage {
    job: ScenarioJob,
    commit_counter: Arc<AtomicUsize>,
}

impl CommitableJob for InMemoryJobMessage {
    fn job(&self) -> &ScenarioJob {
        &self.job
    }

    fn commit(self) -> Result<(), String> {
        self.commit_counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct InMemoryResultPublisher {
    results: Mutex<Vec<ScenarioRunResult>>,
}

#[async_trait]
impl ResultPublisher for InMemoryResultPublisher {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String> {
        self.results.lock().await.push(result.clone());
        Ok(())
    }
}

#[derive(Default)]
struct InMemoryDlqPublisher;

#[async_trait]
impl DlqPublisher for InMemoryDlqPublisher {
    async fn publish_failed_job(
        &self,
        _key: &str,
        _job: &pulse::domain::contracts::FailedScenarioJob,
    ) -> Result<(), String> {
        Ok(())
    }
}

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

fn build_plan(partition_strategy: PartitionKeyStrategy) -> ScenarioExecutionPlan {
    let scenario = Scenario::new(
        "IntegrationScenario",
        vec![Arc::new(NoopStep) as Arc<dyn Step>],
        ScenarioConfig {
            endpoint: "http://127.0.0.1:8080".to_string(),
            scenarios_per_sec: 5.0,
            max_concurrency: 4,
            duration: Duration::from_millis(50),
            repeat: RepeatPolicy::Once,
            partition_key_strategy: partition_strategy,
        },
    );

    let ports = StepPorts {
        default_endpoint: scenario.config.endpoint.clone(),
        dynamic_grpc_gateways: HashMap::new(),
    };

    ScenarioExecutionPlan { scenario, ports }
}

#[tokio::test]
async fn end_to_end_pipeline_publishes_success_result() {
    let (tx, rx) = mpsc::unbounded_channel();
    let commit_counter = Arc::new(AtomicUsize::new(0));

    let elector = Arc::new(NoopLeaderElector);
    let due_store = Arc::new(OnceDueStateStore::default());
    let publisher = Arc::new(InMemoryJobPublisher::new(tx, false, commit_counter.clone()));
    let consumer = Arc::new(InMemoryJobConsumer::new(rx));
    let idempotency = Arc::new(InMemoryIdempotencyStore::default());
    let result_publisher = Arc::new(InMemoryResultPublisher::default());
    let dlq_publisher = Arc::new(InMemoryDlqPublisher);

    let node = PulseNode::new(
        PulseNodeDependencies {
            elector,
            due_store,
            job_publisher: publisher.clone(),
            job_consumer: consumer,
            idempotency_store: idempotency,
            result_publisher: result_publisher.clone(),
            dlq_publisher,
        },
        vec![build_plan(PartitionKeyStrategy::ScenarioId)],
        NodeRuntimeConfig {
            leader_renew_interval: Duration::from_millis(10),
            scheduler_tick_interval: Duration::from_millis(10),
            worker_max_retries: 1,
            worker_retry_base_delay: Duration::from_millis(10),
        },
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    timeout(Duration::from_secs(2), async {
        loop {
            if !result_publisher.results.lock().await.is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for result publication");

    let results = result_publisher.results.lock().await.clone();
    assert_eq!(results.len(), 1, "exactly one run should be executed");
    assert_eq!(results[0].scenario_id, "IntegrationScenario");
    assert_eq!(results[0].status, ScenarioRunStatus::Success);
    assert!(
        results[0].total > 0,
        "run should execute at least one scenario"
    );

    let keys = publisher.published_keys.lock().await.clone();
    assert_eq!(keys.len(), 1, "exactly one job should be published");
    assert_eq!(keys[0], "IntegrationScenario");

    assert_eq!(
        commit_counter.load(Ordering::SeqCst),
        1,
        "processed message should be committed"
    );

    let _ = shutdown_tx.send(true);
    let _ = handle.await;
}

#[tokio::test]
async fn duplicate_jobs_are_deduplicated_by_idempotency_store() {
    let (tx, rx) = mpsc::unbounded_channel();
    let commit_counter = Arc::new(AtomicUsize::new(0));

    let elector = Arc::new(NoopLeaderElector);
    let due_store = Arc::new(OnceDueStateStore::default());
    let publisher = Arc::new(InMemoryJobPublisher::new(tx, true, commit_counter.clone()));
    let consumer = Arc::new(InMemoryJobConsumer::new(rx));
    let idempotency = Arc::new(InMemoryIdempotencyStore::default());
    let result_publisher = Arc::new(InMemoryResultPublisher::default());
    let dlq_publisher = Arc::new(InMemoryDlqPublisher);

    let node = PulseNode::new(
        PulseNodeDependencies {
            elector,
            due_store,
            job_publisher: publisher,
            job_consumer: consumer,
            idempotency_store: idempotency,
            result_publisher: result_publisher.clone(),
            dlq_publisher,
        },
        vec![build_plan(PartitionKeyStrategy::ExecutionKey)],
        NodeRuntimeConfig {
            leader_renew_interval: Duration::from_millis(10),
            scheduler_tick_interval: Duration::from_millis(10),
            worker_max_retries: 1,
            worker_retry_base_delay: Duration::from_millis(10),
        },
    );

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    timeout(Duration::from_secs(2), async {
        loop {
            if commit_counter.load(Ordering::SeqCst) >= 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for duplicate message commits");

    sleep(Duration::from_millis(250)).await;

    let results = result_publisher.results.lock().await.clone();
    assert_eq!(
        results.len(),
        1,
        "duplicate execution key should produce only one result"
    );
    assert_eq!(
        commit_counter.load(Ordering::SeqCst),
        2,
        "both messages should be committed even if one is deduplicated"
    );

    let _ = shutdown_tx.send(true);
    let _ = handle.await;
}
