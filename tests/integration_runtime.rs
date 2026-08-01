use std::collections::{HashMap, HashSet, VecDeque};
use std::future::pending;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use pulse::application::service::{
    CommitableJob, DlqPublisher, JobConsumer, JobPublisher, NodeRuntimeConfig, PulseNode,
    PulseNodeDependencies, ResultPublisher, ScenarioExecutionPlan, execution_plan_fingerprint,
    planned_slice_loads,
};
use pulse::domain::context::ScenarioContext;
use pulse::domain::contracts::{
    CURRENT_CONTRACT_VERSION, FailedScenarioJob, JobLoadConfig, JobSlice, PartitionKeyStrategy,
    PoisonMessageRecord, ScenarioJob, ScenarioRunResult, ScenarioRunStatus, now_unix_ms,
};
use pulse::domain::coordination::{
    ClaimOutcome, CompletedOutcome, CoordinationError, DispatchOutcome, DispatchProgress,
    DispatchSpec, DispatchStore, DispatchWindow, ExecutionClaim, ExecutionLease,
    ExecutionLeaseStore, LeaderElector, LeaderLease, LeadershipOutcome, ReleaseOutcome,
    TerminalOutcome,
};
use pulse::domain::error::{ContractError, PulseError};
use pulse::domain::scenario::{RepeatPolicy, Scenario, ScenarioConfig, Step, StepPorts};
use tokio::sync::{Mutex, watch};
use tokio::time::{sleep, timeout};

fn leader_lease() -> LeaderLease {
    LeaderLease {
        lock_key: "test:leader".to_string(),
        node_id: "test-node".to_string(),
        owner_token: "test-owner".to_string(),
        fencing_token: 1,
        expires_at_unix_ms: u64::MAX,
        ttl: Duration::from_secs(30),
    }
}

#[derive(Default)]
struct StableLeaderElector;

#[async_trait]
impl LeaderElector for StableLeaderElector {
    async fn acquire_or_renew(
        &self,
        current: Option<&LeaderLease>,
    ) -> Result<LeadershipOutcome, CoordinationError> {
        Ok(match current {
            Some(lease) => LeadershipOutcome::Renewed(lease.clone()),
            None => LeadershipOutcome::Acquired(leader_lease()),
        })
    }

    async fn relinquish(&self, _lease: &LeaderLease) -> Result<(), CoordinationError> {
        Ok(())
    }
}

#[derive(Default)]
struct FollowerElector;

#[async_trait]
impl LeaderElector for FollowerElector {
    async fn acquire_or_renew(
        &self,
        _current: Option<&LeaderLease>,
    ) -> Result<LeadershipOutcome, CoordinationError> {
        Ok(LeadershipOutcome::Follower {
            retry_after: Duration::from_millis(10),
        })
    }

    async fn relinquish(&self, _lease: &LeaderLease) -> Result<(), CoordinationError> {
        Ok(())
    }
}

#[derive(Default)]
struct LeaseLossElector {
    calls: AtomicUsize,
}

#[async_trait]
impl LeaderElector for LeaseLossElector {
    async fn acquire_or_renew(
        &self,
        current: Option<&LeaderLease>,
    ) -> Result<LeadershipOutcome, CoordinationError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 && current.is_none() {
            Ok(LeadershipOutcome::Acquired(leader_lease()))
        } else {
            Ok(LeadershipOutcome::Follower {
                retry_after: Duration::from_millis(10),
            })
        }
    }

    async fn relinquish(&self, _lease: &LeaderLease) -> Result<(), CoordinationError> {
        Ok(())
    }
}

#[derive(Default)]
struct MemoryDispatchStore {
    state: Mutex<Option<DispatchState>>,
    prepare_calls: AtomicUsize,
}

struct DispatchState {
    window: DispatchWindow,
    acknowledged: HashSet<u32>,
    complete: bool,
}

impl MemoryDispatchStore {
    async fn acknowledged(&self) -> HashSet<u32> {
        self.state
            .lock()
            .await
            .as_ref()
            .map(|state| state.acknowledged.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl DispatchStore for MemoryDispatchStore {
    async fn prepare_window(
        &self,
        spec: &DispatchSpec,
        _leader: &LeaderLease,
    ) -> Result<DispatchOutcome, CoordinationError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().await;
        if state.is_none() {
            let window_id = format!(
                "v{}:{}:window-1:n{}",
                spec.contract_version, spec.scenario_id, spec.total_slices
            );
            *state = Some(DispatchState {
                window: DispatchWindow {
                    scenario_id: spec.scenario_id.clone(),
                    window_id: window_id.clone(),
                    run_id: window_id,
                    scheduled_at_unix_ms: now_unix_ms(),
                    contract_version: spec.contract_version,
                    total_slices: spec.total_slices,
                    plan_fingerprint: spec.plan_fingerprint.clone(),
                    missing_slices: Vec::new(),
                },
                acknowledged: HashSet::new(),
                complete: false,
            });
        }

        let state = state.as_ref().expect("dispatch state was initialized");
        if state.window.scenario_id != spec.scenario_id
            || state.window.total_slices != spec.total_slices
            || state.window.plan_fingerprint != spec.plan_fingerprint
        {
            return Err(CoordinationError::invalid_state(
                "test_prepare_window",
                "dispatch plan changed",
            ));
        }
        if state.complete {
            return Ok(DispatchOutcome::Finished);
        }

        let mut window = state.window.clone();
        window.missing_slices = (0..window.total_slices)
            .filter(|index| !state.acknowledged.contains(index))
            .collect();
        Ok(DispatchOutcome::Ready(window))
    }

    async fn ack_slice(
        &self,
        window: &DispatchWindow,
        slice_index: u32,
        _leader: &LeaderLease,
    ) -> Result<DispatchProgress, CoordinationError> {
        let mut state = self.state.lock().await;
        let state = state.as_mut().ok_or_else(|| {
            CoordinationError::invalid_state("test_ack_slice", "window is absent")
        })?;
        if state.window.window_id != window.window_id || slice_index >= window.total_slices {
            return Err(CoordinationError::invalid_state(
                "test_ack_slice",
                "window or slice does not match",
            ));
        }
        state.acknowledged.insert(slice_index);
        let remaining = window
            .total_slices
            .saturating_sub(state.acknowledged.len() as u32);
        if remaining == 0 {
            state.complete = true;
            Ok(DispatchProgress::Complete)
        } else {
            Ok(DispatchProgress::Pending {
                remaining_slices: remaining,
            })
        }
    }
}

#[derive(Default)]
struct ExecutionState {
    running: HashMap<(String, u32), ExecutionLease>,
    completed: HashMap<(String, u32), CompletedOutcome>,
}

#[derive(Default)]
struct MemoryExecutionLeaseStore {
    state: Mutex<ExecutionState>,
    claim_calls: AtomicUsize,
    complete_calls: AtomicUsize,
    claim_unavailable: AtomicBool,
    completion_unavailable: AtomicBool,
}

impl MemoryExecutionLeaseStore {
    fn unavailable_on_claim() -> Self {
        Self {
            claim_unavailable: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn unavailable_on_completion() -> Self {
        Self {
            completion_unavailable: AtomicBool::new(true),
            ..Self::default()
        }
    }

    fn with_completed(execution_key: &str, attempt: u32, outcome: TerminalOutcome) -> Self {
        let mut state = ExecutionState::default();
        state.completed.insert(
            (execution_key.to_string(), attempt),
            CompletedOutcome {
                outcome,
                completed_at_unix_ms: now_unix_ms(),
            },
        );
        Self {
            state: Mutex::new(state),
            ..Self::default()
        }
    }
}

#[async_trait]
impl ExecutionLeaseStore for MemoryExecutionLeaseStore {
    async fn claim(&self, claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
        let call = self.claim_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.claim_unavailable.load(Ordering::SeqCst) {
            return Err(CoordinationError::unavailable(
                "test_execution_claim",
                "simulated Redis outage",
            ));
        }

        let key = (claim.execution_key.clone(), claim.attempt);
        let mut state = self.state.lock().await;
        if let Some(completed) = state.completed.get(&key) {
            return Ok(ClaimOutcome::AlreadyCompleted(completed.clone()));
        }
        if let Some(running) = state.running.get(&key) {
            return Ok(ClaimOutcome::Busy {
                retry_after: running.ttl,
            });
        }
        let lease = ExecutionLease {
            execution_key: claim.execution_key.clone(),
            attempt: claim.attempt,
            owner_token: format!("worker-{call}"),
            expires_at_unix_ms: now_unix_ms() + 30_000,
            ttl: Duration::from_secs(30),
            recovered: false,
        };
        state.running.insert(key, lease.clone());
        Ok(ClaimOutcome::Acquired(lease))
    }

    async fn renew(&self, lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
        let state = self.state.lock().await;
        let current = state
            .running
            .get(&(lease.execution_key.clone(), lease.attempt));
        if current.is_none_or(|current| current.owner_token != lease.owner_token) {
            return Err(CoordinationError::stale_owner("test_execution_renew"));
        }
        Ok(lease.clone())
    }

    async fn complete(
        &self,
        lease: &ExecutionLease,
        outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        if self.completion_unavailable.load(Ordering::SeqCst) {
            return Err(CoordinationError::unavailable(
                "test_execution_complete",
                "simulated Redis outage after output acknowledgement",
            ));
        }

        let key = (lease.execution_key.clone(), lease.attempt);
        let mut state = self.state.lock().await;
        let current = state.running.get(&key);
        if current.is_none_or(|current| current.owner_token != lease.owner_token) {
            return Err(CoordinationError::stale_owner("test_execution_complete"));
        }
        state.running.remove(&key);
        let completed = CompletedOutcome {
            outcome,
            completed_at_unix_ms: now_unix_ms(),
        };
        state.completed.insert(key, completed.clone());
        Ok(completed)
    }

    async fn release(&self, lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError> {
        let key = (lease.execution_key.clone(), lease.attempt);
        let mut state = self.state.lock().await;
        match state.running.get(&key) {
            Some(current) if current.owner_token == lease.owner_token => {
                state.running.remove(&key);
                Ok(ReleaseOutcome::Released)
            }
            Some(_) => Err(CoordinationError::stale_owner("test_execution_release")),
            None => Ok(ReleaseOutcome::AlreadyAbsent),
        }
    }
}

struct BusyThenCompletedExecutionLeaseStore {
    claim_calls: AtomicUsize,
    retry_after: Duration,
}

#[derive(Clone, Copy)]
enum RenewalFailure {
    StaleOwner,
    Unavailable,
}

struct RejectingRenewExecutionLeaseStore {
    failure: RenewalFailure,
    claim_calls: AtomicUsize,
    renew_calls: AtomicUsize,
    complete_calls: AtomicUsize,
}

struct SlowClaimExecutionLeaseStore {
    delay: Duration,
    ttl: Duration,
    claim_returned: AtomicBool,
}

#[async_trait]
impl ExecutionLeaseStore for SlowClaimExecutionLeaseStore {
    async fn claim(&self, claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
        sleep(self.delay).await;
        self.claim_returned.store(true, Ordering::SeqCst);
        Ok(ClaimOutcome::Acquired(ExecutionLease {
            execution_key: claim.execution_key.clone(),
            attempt: claim.attempt,
            owner_token: "slow-claim-owner".to_string(),
            expires_at_unix_ms: now_unix_ms() + self.ttl.as_millis(),
            ttl: self.ttl,
            recovered: false,
        }))
    }

    async fn renew(&self, _lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
        Err(CoordinationError::invalid_state(
            "test_execution_renew",
            "an exhausted claim budget must never start renewal",
        ))
    }

    async fn complete(
        &self,
        _lease: &ExecutionLease,
        _outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError> {
        Err(CoordinationError::invalid_state(
            "test_execution_complete",
            "an exhausted claim budget must never complete",
        ))
    }

    async fn release(&self, _lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError> {
        Ok(ReleaseOutcome::AlreadyAbsent)
    }
}

impl RejectingRenewExecutionLeaseStore {
    fn new(failure: RenewalFailure) -> Self {
        Self {
            failure,
            claim_calls: AtomicUsize::new(0),
            renew_calls: AtomicUsize::new(0),
            complete_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ExecutionLeaseStore for RejectingRenewExecutionLeaseStore {
    async fn claim(&self, claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
        self.claim_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ClaimOutcome::Acquired(ExecutionLease {
            execution_key: claim.execution_key.clone(),
            attempt: claim.attempt,
            owner_token: "stale-before-publication".to_string(),
            expires_at_unix_ms: now_unix_ms() + 30_000,
            ttl: Duration::from_secs(30),
            recovered: false,
        }))
    }

    async fn renew(&self, _lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
        self.renew_calls.fetch_add(1, Ordering::SeqCst);
        Err(match self.failure {
            RenewalFailure::StaleOwner => CoordinationError::stale_owner("test_execution_renew"),
            RenewalFailure::Unavailable => {
                CoordinationError::unavailable("test_execution_renew", "simulated Redis outage")
            }
        })
    }

    async fn complete(
        &self,
        _lease: &ExecutionLease,
        _outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Err(CoordinationError::stale_owner("test_execution_complete"))
    }

    async fn release(&self, _lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError> {
        Ok(ReleaseOutcome::AlreadyAbsent)
    }
}

impl BusyThenCompletedExecutionLeaseStore {
    fn new(retry_after: Duration) -> Self {
        Self {
            claim_calls: AtomicUsize::new(0),
            retry_after,
        }
    }
}

#[async_trait]
impl ExecutionLeaseStore for BusyThenCompletedExecutionLeaseStore {
    async fn claim(&self, _claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError> {
        if self.claim_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(ClaimOutcome::Busy {
                retry_after: self.retry_after,
            })
        } else {
            Ok(ClaimOutcome::AlreadyCompleted(CompletedOutcome {
                outcome: TerminalOutcome::ResultPublished,
                completed_at_unix_ms: now_unix_ms(),
            }))
        }
    }

    async fn renew(&self, _lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError> {
        Err(CoordinationError::invalid_state(
            "test_execution_renew",
            "busy duplicate never owns the lease",
        ))
    }

    async fn complete(
        &self,
        _lease: &ExecutionLease,
        _outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError> {
        Err(CoordinationError::invalid_state(
            "test_execution_complete",
            "busy duplicate never owns the lease",
        ))
    }

    async fn release(&self, _lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError> {
        Err(CoordinationError::invalid_state(
            "test_execution_release",
            "busy duplicate never owns the lease",
        ))
    }
}

#[derive(Clone)]
struct TestJobMessage {
    decoded: Result<ScenarioJob, ContractError>,
    commits: Arc<AtomicUsize>,
    commit_fails: bool,
    topic: String,
    partition: i32,
    offset: i64,
}

impl TestJobMessage {
    fn valid(job: ScenarioJob, commits: Arc<AtomicUsize>) -> Self {
        Self {
            decoded: Ok(job),
            commits,
            commit_fails: false,
            topic: "test.jobs".to_string(),
            partition: 0,
            offset: 7,
        }
    }

    fn poison(commits: Arc<AtomicUsize>) -> Self {
        Self {
            decoded: Err(ContractError::Malformed("not valid JSON".to_string())),
            commits,
            commit_fails: false,
            topic: "test.jobs".to_string(),
            partition: 2,
            offset: 42,
        }
    }
}

#[async_trait]
impl CommitableJob for TestJobMessage {
    fn job(&self) -> Result<&ScenarioJob, ContractError> {
        self.decoded.as_ref().map_err(Clone::clone)
    }

    fn poison_record(&self, reason: String) -> PoisonMessageRecord {
        PoisonMessageRecord {
            schema_version: CURRENT_CONTRACT_VERSION,
            event_id: format!("poison:{}:{}:{}", self.topic, self.partition, self.offset),
            failed_at_unix_ms: now_unix_ms(),
            source_topic: self.topic.clone(),
            source_partition: self.partition,
            source_offset: self.offset,
            source_key_base64: None,
            source_key_original_bytes: None,
            source_key_truncated: false,
            payload_base64: Some("bm90IHZhbGlkIEpTT04=".to_string()),
            payload_original_bytes: Some(14),
            payload_truncated: false,
            reason,
        }
    }

    async fn commit(self) -> Result<(), String> {
        if self.commit_fails {
            return Err("simulated synchronous commit failure".to_string());
        }
        self.commits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct QueueJobConsumer {
    records: Mutex<VecDeque<TestJobMessage>>,
    delivered: AtomicUsize,
}

impl QueueJobConsumer {
    fn new(records: impl IntoIterator<Item = TestJobMessage>) -> Self {
        Self {
            records: Mutex::new(records.into_iter().collect()),
            delivered: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl JobConsumer for QueueJobConsumer {
    type Item = TestJobMessage;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        if let Some(record) = self.records.lock().await.pop_front() {
            self.delivered.fetch_add(1, Ordering::SeqCst);
            return Ok(Some(record));
        }
        pending::<Result<Option<Self::Item>, String>>().await
    }
}

#[derive(Default)]
struct PendingJobConsumer;

#[async_trait]
impl JobConsumer for PendingJobConsumer {
    type Item = TestJobMessage;

    async fn recv(&self) -> Result<Option<Self::Item>, String> {
        pending::<Result<Option<Self::Item>, String>>().await
    }
}

struct CapturingJobPublisher {
    jobs: Mutex<Vec<ScenarioJob>>,
    fail_slice_once: Option<u32>,
    failed_once: AtomicBool,
}

struct PublishCancellationGuard(Arc<AtomicBool>);

impl Drop for PublishCancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct BlockingJobPublisher {
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

#[async_trait]
impl JobPublisher for BlockingJobPublisher {
    async fn publish_job(&self, _key: &str, _job: &ScenarioJob) -> Result<(), String> {
        let _guard = PublishCancellationGuard(self.cancelled.clone());
        self.started.store(true, Ordering::SeqCst);
        pending::<Result<(), String>>().await
    }
}

impl CapturingJobPublisher {
    fn succeeds() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            fail_slice_once: None,
            failed_once: AtomicBool::new(false),
        }
    }

    fn fail_slice_once(slice: u32) -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            fail_slice_once: Some(slice),
            failed_once: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl JobPublisher for CapturingJobPublisher {
    async fn publish_job(&self, _key: &str, job: &ScenarioJob) -> Result<(), String> {
        self.jobs.lock().await.push(job.clone());
        if self.fail_slice_once == Some(job.slice.index)
            && !self.failed_once.swap(true, Ordering::SeqCst)
        {
            return Err("simulated Kafka publication failure".to_string());
        }
        Ok(())
    }
}

#[derive(Default)]
struct FailingJobPublisher {
    jobs: Mutex<Vec<ScenarioJob>>,
    attempts: AtomicUsize,
}

#[async_trait]
impl JobPublisher for FailingJobPublisher {
    async fn publish_job(&self, _key: &str, job: &ScenarioJob) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.jobs.lock().await.push(job.clone());
        Err("simulated retry topic outage".to_string())
    }
}

#[derive(Default)]
struct CapturingResultPublisher {
    results: Mutex<Vec<ScenarioRunResult>>,
    attempts: AtomicUsize,
    fail: AtomicBool,
}

impl CapturingResultPublisher {
    fn failing() -> Self {
        Self {
            fail: AtomicBool::new(true),
            ..Self::default()
        }
    }
}

#[async_trait]
impl ResultPublisher for CapturingResultPublisher {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err("simulated result topic outage".to_string());
        }
        self.results.lock().await.push(result.clone());
        Ok(())
    }
}

struct FlakyResultPublisher {
    failures_remaining: AtomicUsize,
    attempts: AtomicUsize,
    results: Mutex<Vec<ScenarioRunResult>>,
}

impl FlakyResultPublisher {
    fn new(failures: usize) -> Self {
        Self {
            failures_remaining: AtomicUsize::new(failures),
            attempts: AtomicUsize::new(0),
            results: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ResultPublisher for FlakyResultPublisher {
    async fn publish_result(&self, result: &ScenarioRunResult) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err("simulated transient result topic outage".to_string());
        }
        self.results.lock().await.push(result.clone());
        Ok(())
    }
}

#[derive(Default)]
struct CapturingDlqPublisher {
    failed_jobs: Mutex<Vec<FailedScenarioJob>>,
    poison_records: Mutex<Vec<PoisonMessageRecord>>,
    attempts: AtomicUsize,
    fail: AtomicBool,
}

impl CapturingDlqPublisher {
    fn failing() -> Self {
        Self {
            fail: AtomicBool::new(true),
            ..Self::default()
        }
    }
}

#[async_trait]
impl DlqPublisher for CapturingDlqPublisher {
    async fn publish_failed_job(&self, _key: &str, job: &FailedScenarioJob) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err("simulated DLQ outage".to_string());
        }
        self.failed_jobs.lock().await.push(job.clone());
        Ok(())
    }

    async fn publish_poison(&self, record: &PoisonMessageRecord) -> Result<(), String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err("simulated DLQ outage".to_string());
        }
        self.poison_records.lock().await.push(record.clone());
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

struct CountingStep {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Step for CountingStep {
    fn name(&self) -> &str {
        "counting"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct TargetFailureStep {
    calls: Arc<AtomicUsize>,
}

struct InfrastructureFailureStep {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Step for InfrastructureFailureStep {
    fn name(&self) -> &str {
        "infrastructure_failure"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(PulseError::TransientInfrastructure(
            "fixture dependency unavailable".to_string(),
        ))
    }
}

#[async_trait]
impl Step for TargetFailureStep {
    fn name(&self) -> &str {
        "target_failure"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(PulseError::TargetStatus {
            code: "unavailable".to_string(),
            message: "fixture rejected request".to_string(),
        })
    }
}

struct BlockingStep {
    started: Arc<AtomicBool>,
}

struct PanickingStep;

struct MixedInvariantAndInfrastructureStep {
    calls: AtomicUsize,
}

impl MixedInvariantAndInfrastructureStep {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Step for MixedInvariantAndInfrastructureStep {
    fn name(&self) -> &str {
        "mixed_invariant_and_infrastructure"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("deterministic mixed-failure integration panic");
        }
        Err(PulseError::TransientInfrastructure(
            "concurrent fixture dependency unavailable".to_string(),
        ))
    }
}

#[async_trait]
impl Step for PanickingStep {
    fn name(&self) -> &str {
        "panicking"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        panic!("deterministic integration scenario panic")
    }
}

#[async_trait]
impl Step for BlockingStep {
    fn name(&self) -> &str {
        "blocking"
    }

    async fn execute(
        &self,
        _ctx: &mut ScenarioContext,
        _ports: &StepPorts,
    ) -> Result<(), PulseError> {
        self.started.store(true, Ordering::SeqCst);
        pending::<Result<(), PulseError>>().await
    }
}

fn build_plan_with(rate: f64, concurrency: usize, step: Arc<dyn Step>) -> ScenarioExecutionPlan {
    let scenario = Scenario::new(
        "IntegrationScenario",
        vec![step],
        ScenarioConfig {
            endpoint: "http://127.0.0.1:50051".to_string(),
            scenarios_per_sec: rate,
            max_concurrency: concurrency,
            duration: Duration::from_millis(50),
            repeat: RepeatPolicy::Once,
            partition_key_strategy: PartitionKeyStrategy::ExecutionKey,
        },
    );
    let ports = StepPorts {
        default_endpoint: scenario.config.endpoint.clone(),
        dynamic_grpc_gateways: HashMap::new(),
    };
    ScenarioExecutionPlan {
        scenario,
        ports,
        execution_semantics_fingerprint: "integration-runtime-v1".to_string(),
    }
}

fn build_plan(step: Arc<dyn Step>) -> ScenarioExecutionPlan {
    build_plan_with(5.0, 4, step)
}

fn valid_job(execution_key: &str) -> ScenarioJob {
    ScenarioJob {
        schema_version: 1,
        scenario_id: "IntegrationScenario".to_string(),
        run_id: "run-1".to_string(),
        execution_key: execution_key.to_string(),
        plan_fingerprint: String::new(),
        scheduled_at_unix_ms: now_unix_ms(),
        not_before_unix_ms: 0,
        slice: JobSlice { index: 0, total: 1 },
        load: JobLoadConfig {
            scenarios_per_sec: 5.0,
            duration: Duration::from_millis(50),
            max_concurrency: 4,
            startup_burst: 1,
        },
        attempt: 0,
        max_retries: 2,
    }
}

fn current_job_for_plan(
    execution_key: &str,
    plan: &ScenarioExecutionPlan,
    config: &NodeRuntimeConfig,
) -> ScenarioJob {
    let mut job = valid_job(execution_key);
    job.schema_version = CURRENT_CONTRACT_VERSION;
    job.load = planned_slice_loads(&plan.scenario, config.startup_burst)
        .into_iter()
        .next()
        .expect("test scenario has one deterministic slice");
    job.plan_fingerprint =
        execution_plan_fingerprint(plan, 1, config.startup_burst, config.worker_max_retries);
    job
}

fn runtime_config() -> NodeRuntimeConfig {
    NodeRuntimeConfig {
        leader_renew_interval: Duration::from_millis(10),
        scheduler_tick_interval: Duration::from_millis(10),
        worker_max_retries: 2,
        worker_retry_base_delay: Duration::from_millis(20),
        worker_retry_max_delay: Duration::from_millis(50),
        worker_queue_capacity: 4,
        execution_renew_interval: Duration::from_secs(5),
        shutdown_drain_timeout: Duration::from_millis(30),
        max_processing_interval: Duration::from_secs(1),
        max_job_duration: Duration::from_secs(1),
        max_scenarios_per_sec: 100.0,
        max_concurrency: 32,
        scenario_timeout: Some(Duration::from_millis(500)),
        startup_burst: 1,
    }
}

async fn wait_until(description: &str, predicate: impl Fn() -> bool) {
    timeout(Duration::from_secs(2), async {
        while !predicate() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}

async fn stop_node(shutdown_tx: watch::Sender<bool>, handle: tokio::task::JoinHandle<()>) {
    let _ = shutdown_tx.send(true);
    timeout(Duration::from_secs(2), handle)
        .await
        .expect("node did not stop within its shutdown deadline")
        .expect("node task panicked");
}

#[tokio::test]
async fn successful_result_is_published_before_source_commit() {
    let commits = Arc::new(AtomicUsize::new(0));
    let consumer = Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
        valid_job("success"),
        commits.clone(),
    )]));
    let results = Arc::new(CapturingResultPublisher::default());
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: consumer.clone(),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("successful source commit", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    let published = results.results.lock().await.clone();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].status, ScenarioRunStatus::Success);
    assert_eq!(leases.complete_calls.load(Ordering::SeqCst), 1);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn v2_plan_or_slice_load_drift_is_dead_lettered_without_target_traffic() {
    for mutate_fingerprint in [false, true] {
        let commits = Arc::new(AtomicUsize::new(0));
        let step_calls = Arc::new(AtomicUsize::new(0));
        let plan = build_plan(Arc::new(CountingStep {
            calls: step_calls.clone(),
        }));
        let config = runtime_config();
        let execution_key = if mutate_fingerprint {
            "plan-fingerprint-drift"
        } else {
            "slice-load-drift"
        };
        let mut job = current_job_for_plan(execution_key, &plan, &config);
        if mutate_fingerprint {
            job.plan_fingerprint = "fnv128:stale-plan".to_string();
        } else {
            job.load.scenarios_per_sec += 0.5;
        }
        let dlq = Arc::new(CapturingDlqPublisher::default());
        let node = PulseNode::new(
            PulseNodeDependencies {
                elector: Arc::new(FollowerElector),
                due_store: Arc::new(MemoryDispatchStore::default()),
                job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
                job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                    job,
                    commits.clone(),
                )])),
                idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
                result_publisher: Arc::new(CapturingResultPublisher::default()),
                dlq_publisher: dlq.clone(),
            },
            vec![plan],
            config,
        );
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let handle = tokio::spawn(node.run(shutdown_rx));

        wait_until("plan-drift source commit", || {
            commits.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(step_calls.load(Ordering::SeqCst), 0);
        let failed = dlq.failed_jobs.lock().await.clone();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].reason.contains(if mutate_fingerprint {
            "fingerprint"
        } else {
            "deterministic local slice plan"
        }));

        stop_node(shutdown_tx, handle).await;
    }
}

#[tokio::test]
async fn coordination_error_is_not_a_duplicate_and_source_is_not_committed() {
    let commits = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(MemoryExecutionLeaseStore::unavailable_on_claim());
    let results = Arc::new(CapturingResultPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("claim-outage"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("failed coordination claim", || {
        leases.claim_calls.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn slow_claim_response_cannot_run_target_after_local_lease_budget() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(SlowClaimExecutionLeaseStore {
        delay: Duration::from_millis(50),
        ttl: Duration::from_millis(60),
        claim_returned: AtomicBool::new(false),
    });
    let results = Arc::new(CapturingResultPublisher::default());
    let mut config = runtime_config();
    config.execution_renew_interval = Duration::from_millis(20);
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("slow-claim"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(CountingStep {
            calls: step_calls.clone(),
        }))],
        config,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("slow execution claim response", || {
        leases.claim_returned.load(Ordering::SeqCst)
    })
    .await;
    sleep(Duration::from_millis(10)).await;
    assert_eq!(step_calls.load(Ordering::SeqCst), 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn verified_durable_duplicate_commits_without_reexecution() {
    let commits = Arc::new(AtomicUsize::new(0));
    let job = valid_job("completed-duplicate");
    let leases = Arc::new(MemoryExecutionLeaseStore::with_completed(
        &job.execution_key,
        job.attempt,
        TerminalOutcome::ResultPublished,
    ));
    let results = Arc::new(CapturingResultPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                job,
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("duplicate source commit", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 1);
    assert!(results.results.lock().await.is_empty());

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test(start_paused = true)]
async fn busy_redelivery_waits_until_recheck_before_durable_duplicate_commit() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(BusyThenCompletedExecutionLeaseStore::new(
        Duration::from_millis(25),
    ));
    let results = Arc::new(CapturingResultPublisher::default());
    let mut config = runtime_config();
    config.worker_retry_max_delay = Duration::from_millis(25);
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("busy-redelivery"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(CountingStep {
            calls: step_calls.clone(),
        }))],
        config,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    for _ in 0..100 {
        if leases.claim_calls.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_millis(24)).await;
    tokio::task::yield_now().await;
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 1);
    assert_eq!(commits.load(Ordering::SeqCst), 0);

    tokio::time::advance(Duration::from_millis(1)).await;
    for _ in 0..100 {
        if commits.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 2);
    assert_eq!(step_calls.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());
    assert!(
        !handle.is_finished(),
        "a legitimate busy lease must not fail-stop the worker"
    );

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn result_publication_failure_never_commits_source() {
    let commits = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(CapturingResultPublisher::failing());
    let retry_jobs = Arc::new(CapturingJobPublisher::succeeds());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: retry_jobs.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("result-outage"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    timeout(Duration::from_secs(2), handle)
        .await
        .expect("exhausted result publication did not fail-stop the node")
        .expect("node task panicked");
    assert_eq!(
        results.attempts.load(Ordering::SeqCst),
        3,
        "the complete bounded local result-publication budget must be exhausted"
    );
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(
        retry_jobs.jobs.lock().await.is_empty(),
        "a result-topic outage must not publish a whole-slice retry"
    );
}

#[tokio::test]
async fn stale_owner_is_rechecked_before_result_publication() {
    let commits = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(RejectingRenewExecutionLeaseStore::new(
        RenewalFailure::StaleOwner,
    ));
    let results = Arc::new(CapturingResultPublisher::default());
    let retry_jobs = Arc::new(CapturingJobPublisher::succeeds());
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: retry_jobs.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("stale-before-result"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("stale publication fence did not fail-stop the node");

    assert!(leases.renew_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(leases.complete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(results.attempts.load(Ordering::SeqCst), 0);
    assert!(retry_jobs.jobs.lock().await.is_empty());
    assert!(dlq.failed_jobs.lock().await.is_empty());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transient_result_outage_retries_retained_output_without_reexecuting_target() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(FlakyResultPublisher::new(2));
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("transient-result-outage"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(CountingStep {
            calls: step_calls.clone(),
        }))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("source commit after transient result outage", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    assert_eq!(results.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(results.results.lock().await.len(), 1);
    assert_eq!(step_calls.load(Ordering::SeqCst), 1);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn dlq_publication_failure_never_commits_poison_source() {
    let commits = Arc::new(AtomicUsize::new(0));
    let dlq = Arc::new(CapturingDlqPublisher::failing());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::poison(
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("poison DLQ publication retry", || {
        dlq.attempts.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(commits.load(Ordering::SeqCst), 0);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn malformed_payload_has_deterministic_dlq_disposition() {
    let commits = Arc::new(AtomicUsize::new(0));
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::poison(
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("poison source commit", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    let poison = dlq.poison_records.lock().await.clone();
    assert_eq!(poison.len(), 1);
    assert_eq!(poison[0].event_id, "poison:test.jobs:2:42");
    assert_eq!(poison[0].source_partition, 2);
    assert_eq!(poison[0].source_offset, 42);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn target_failure_is_published_as_measurement_without_whole_slice_retry() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(CapturingResultPublisher::default());
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("target-failure"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(TargetFailureStep {
            calls: step_calls.clone(),
        }))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("target-failure source commit", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    let published = results.results.lock().await.clone();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].status, ScenarioRunStatus::Failed);
    assert_eq!(published[0].failure, 1);
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 1);
    assert_eq!(step_calls.load(Ordering::SeqCst), 1);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn infrastructure_failure_publishes_durable_next_attempt_before_source_commit() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let publisher = Arc::new(CapturingJobPublisher::succeeds());
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let original = valid_job("infrastructure-retry");
    let original_schedule = original.scheduled_at_unix_ms;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: publisher.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                original,
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(InfrastructureFailureStep {
            calls: step_calls.clone(),
        }))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("source commit after durable retry publication", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    let published = publisher.jobs.lock().await.clone();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].execution_key, "infrastructure-retry");
    assert_eq!(published[0].scheduled_at_unix_ms, original_schedule);
    assert_eq!(published[0].attempt, 1);
    assert_eq!(published[0].max_retries, 2);
    assert!(published[0].not_before_unix_ms > now_unix_ms().saturating_sub(100));
    assert_eq!(step_calls.load(Ordering::SeqCst), 1);
    let state = leases.state.lock().await;
    assert_eq!(
        state
            .completed
            .get(&("infrastructure-retry".to_string(), 0))
            .map(|completed| completed.outcome),
        Some(TerminalOutcome::RetryPublished)
    );
    drop(state);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn stale_owner_is_rechecked_before_retry_publication() {
    let commits = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(RejectingRenewExecutionLeaseStore::new(
        RenewalFailure::StaleOwner,
    ));
    let retries = Arc::new(CapturingJobPublisher::succeeds());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: retries.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("stale-before-retry"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(InfrastructureFailureStep {
            calls: Arc::new(AtomicUsize::new(0)),
        }))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("stale retry fence did not fail-stop the node");

    assert!(leases.renew_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(leases.complete_calls.load(Ordering::SeqCst), 0);
    assert!(retries.jobs.lock().await.is_empty());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn retry_publication_failure_keeps_source_uncommitted_and_reuses_identity() {
    let commits = Arc::new(AtomicUsize::new(0));
    let publisher = Arc::new(FailingJobPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: publisher.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("retry-publish-outage"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(InfrastructureFailureStep {
            calls: Arc::new(AtomicUsize::new(0)),
        }))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("retry publication attempts", || {
        publisher.attempts.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    let jobs = publisher.jobs.lock().await.clone();
    assert!(jobs.len() >= 2);
    assert!(jobs.iter().all(|job| job.attempt == 1));
    assert!(
        jobs.iter()
            .all(|job| job.execution_key == "retry-publish-outage")
    );
    assert!(
        jobs.windows(2)
            .all(|pair| pair[0].not_before_unix_ms == pair[1].not_before_unix_ms)
    );

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn exhausted_infrastructure_retry_is_dead_lettered_without_another_job() {
    let commits = Arc::new(AtomicUsize::new(0));
    let publisher = Arc::new(CapturingJobPublisher::succeeds());
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let mut job = valid_job("retry-exhausted");
    job.attempt = job.max_retries;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: publisher.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                job,
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(InfrastructureFailureStep {
            calls: Arc::new(AtomicUsize::new(0)),
        }))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("source commit after exhausted retry DLQ", || {
        commits.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(publisher.jobs.lock().await.is_empty());
    let failed = dlq.failed_jobs.lock().await.clone();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].attempt, 2);
    assert!(failed[0].reason.contains("attempts exhausted"));
    let state = leases.state.lock().await;
    assert_eq!(
        state
            .completed
            .get(&("retry-exhausted".to_string(), 2))
            .map(|completed| completed.outcome),
        Some(TerminalOutcome::DeadLetterPublished)
    );
    drop(state);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn stale_owner_is_rechecked_before_leased_dlq_publication() {
    let commits = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(RejectingRenewExecutionLeaseStore::new(
        RenewalFailure::StaleOwner,
    ));
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let mut job = valid_job("stale-before-dlq");
    job.attempt = job.max_retries;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                job,
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(InfrastructureFailureStep {
            calls: Arc::new(AtomicUsize::new(0)),
        }))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("stale DLQ fence did not fail-stop the node");

    assert!(leases.renew_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(leases.complete_calls.load(Ordering::SeqCst), 0);
    assert!(dlq.failed_jobs.lock().await.is_empty());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shutdown_during_retry_deferral_never_claims_or_commits_source() {
    let commits = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let consumer = Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
        {
            let mut job = valid_job("deferred-shutdown");
            job.attempt = 1;
            job.not_before_unix_ms = now_unix_ms() + 500;
            job
        },
        commits.clone(),
    )]));
    let mut config = runtime_config();
    config.worker_retry_max_delay = Duration::from_millis(600);
    config.max_processing_interval = Duration::from_secs(2);
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: consumer.clone(),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        config,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("deferred retry source delivery", || {
        consumer.delivered.load(Ordering::SeqCst) == 1
    })
    .await;
    stop_node(shutdown_tx, handle).await;
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn renewal_dependency_error_never_publishes_follow_on_work() {
    let commits = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let leases = Arc::new(RejectingRenewExecutionLeaseStore::new(
        RenewalFailure::Unavailable,
    ));
    let retries = Arc::new(CapturingJobPublisher::succeeds());
    let results = Arc::new(CapturingResultPublisher::default());
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let mut config = runtime_config();
    config.execution_renew_interval = Duration::from_millis(10);
    config.scenario_timeout = None;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: retries.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("renewal-outage"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(BlockingStep {
            started: started.clone(),
        }))],
        config,
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("renewal outage did not fail-stop the node");

    assert!(started.load(Ordering::SeqCst));
    assert!(leases.renew_calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(leases.complete_calls.load(Ordering::SeqCst), 0);
    assert!(retries.jobs.lock().await.is_empty());
    assert!(results.results.lock().await.is_empty());
    assert!(dlq.failed_jobs.lock().await.is_empty());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn already_requested_shutdown_never_accepts_queued_work() {
    let commits = Arc::new(AtomicUsize::new(0));
    let step_calls = Arc::new(AtomicUsize::new(0));
    let leases = Arc::new(MemoryExecutionLeaseStore::default());
    let consumer = Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
        valid_job("queued-at-shutdown"),
        commits.clone(),
    )]));
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: consumer.clone(),
            idempotency_store: leases.clone(),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(CountingStep {
            calls: step_calls.clone(),
        }))],
        runtime_config(),
    );

    let (_shutdown_tx, shutdown_rx) = watch::channel(true);
    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("node did not stop when shutdown was already requested");

    assert_eq!(consumer.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(leases.claim_calls.load(Ordering::SeqCst), 0);
    assert_eq!(step_calls.load(Ordering::SeqCst), 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn output_is_retained_while_completion_record_retries_without_source_commit() {
    let commits = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(CapturingResultPublisher::default());
    let leases = Arc::new(MemoryExecutionLeaseStore::unavailable_on_completion());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("completion-outage"),
                commits.clone(),
            )])),
            idempotency_store: leases.clone(),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("owner-checked completion retry", || {
        leases.complete_calls.load(Ordering::SeqCst) >= 2
    })
    .await;
    let published = results.results.lock().await.clone();
    assert_eq!(
        published.len(),
        1,
        "an acknowledged result must be retained while Redis completion retries"
    );
    assert_eq!(published[0].event_id, "completion-outage:attempt-0:result");
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(leases.complete_calls.load(Ordering::SeqCst) >= 2);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn consumer_keeps_polling_while_first_job_waits_for_settlement_retry() {
    let commits = Arc::new(AtomicUsize::new(0));
    let consumer = Arc::new(QueueJobConsumer::new([
        TestJobMessage::valid(valid_job("first-unsettled"), commits.clone()),
        TestJobMessage::valid(valid_job("second-prefetched"), commits.clone()),
    ]));
    let results = Arc::new(CapturingResultPublisher::failing());
    let mut config = runtime_config();
    config.worker_retry_base_delay = Duration::from_millis(200);
    config.worker_retry_max_delay = Duration::from_millis(200);
    config.worker_queue_capacity = 1;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: consumer.clone(),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        config,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("second job to be polled into bounded queue", || {
        consumer.delivered.load(Ordering::SeqCst) == 2
    })
    .await;
    assert_eq!(commits.load(Ordering::SeqCst), 0);

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn partial_scheduler_publication_retries_only_missing_slice() {
    let dispatch = Arc::new(MemoryDispatchStore::default());
    let publisher = Arc::new(CapturingJobPublisher::fail_slice_once(1));
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(StableLeaderElector),
            due_store: dispatch.clone(),
            job_publisher: publisher.clone(),
            job_consumer: Arc::new(PendingJobConsumer),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan_with(40.0, 3, Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    timeout(Duration::from_secs(2), async {
        loop {
            if dispatch.acknowledged().await == HashSet::from([0, 1]) {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("timed out waiting for incomplete window recovery");

    let jobs = publisher.jobs.lock().await.clone();
    let slice_zero = jobs.iter().filter(|job| job.slice.index == 0).count();
    let slice_one = jobs.iter().filter(|job| job.slice.index == 1).count();
    assert_eq!(slice_zero, 1, "acknowledged slice must not be republished");
    assert_eq!(slice_one, 2, "failed slice must retain identity and retry");
    let slice_one_keys: HashSet<_> = jobs
        .iter()
        .filter(|job| job.slice.index == 1)
        .map(|job| job.execution_key.as_str())
        .collect();
    assert_eq!(slice_one_keys.len(), 1);
    let slice_one_payloads: Vec<_> = jobs.iter().filter(|job| job.slice.index == 1).collect();
    assert_eq!(slice_one_payloads.len(), 2);
    assert_eq!(
        serde_json::to_vec(slice_one_payloads[0]).expect("serialize first recovered slice"),
        serde_json::to_vec(slice_one_payloads[1]).expect("serialize second recovered slice")
    );
    assert_eq!(
        slice_one_payloads[0].max_retries,
        runtime_config().worker_max_retries
    );

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn leadership_loss_cancels_publication_before_dispatch_acknowledgement() {
    let dispatch = Arc::new(MemoryDispatchStore::default());
    let publication_started = Arc::new(AtomicBool::new(false));
    let publication_cancelled = Arc::new(AtomicBool::new(false));
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(LeaseLossElector::default()),
            due_store: dispatch.clone(),
            job_publisher: Arc::new(BlockingJobPublisher {
                started: publication_started.clone(),
                cancelled: publication_cancelled.clone(),
            }),
            job_consumer: Arc::new(PendingJobConsumer),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: Arc::new(CapturingResultPublisher::default()),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(NoopStep))],
        runtime_config(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("leader publication attempt", || {
        publication_started.load(Ordering::SeqCst)
    })
    .await;
    wait_until("publication cancellation after lease loss", || {
        publication_cancelled.load(Ordering::SeqCst)
    })
    .await;
    assert!(
        dispatch.acknowledged().await.is_empty(),
        "a publication without a broker acknowledgement must remain missing"
    );

    stop_node(shutdown_tx, handle).await;
}

#[tokio::test]
async fn shutdown_drain_expiry_leaves_running_source_uncommitted() {
    let commits = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let results = Arc::new(CapturingResultPublisher::default());
    let mut config = runtime_config();
    config.shutdown_drain_timeout = Duration::from_millis(25);
    config.scenario_timeout = None;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("shutdown-running"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(BlockingStep {
            started: started.clone(),
        }))],
        config,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    wait_until("blocking scenario to start", || {
        started.load(Ordering::SeqCst)
    })
    .await;
    stop_node(shutdown_tx, handle).await;
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());
}

#[tokio::test]
async fn kafka_safe_processing_ceiling_is_fail_stop_and_never_commits_source() {
    let commits = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicBool::new(false));
    let results = Arc::new(CapturingResultPublisher::default());
    let mut config = runtime_config();
    config.max_processing_interval = Duration::from_millis(50);
    config.scenario_timeout = None;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("processing-ceiling"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(BlockingStep {
            started: started.clone(),
        }))],
        config,
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let handle = tokio::spawn(node.run(shutdown_rx));

    timeout(Duration::from_secs(2), handle)
        .await
        .expect("processing ceiling did not stop the node")
        .expect("node task panicked");
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());
}

#[tokio::test]
async fn scenario_task_panic_is_fail_stop_and_never_publishes_or_commits() {
    let commits = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(CapturingResultPublisher::default());
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: Arc::new(CapturingJobPublisher::succeeds()),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                valid_job("scenario-task-panic"),
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: Arc::new(CapturingDlqPublisher::default()),
        },
        vec![build_plan(Arc::new(PanickingStep))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("scenario task panic did not fail-stop the node");

    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(results.results.lock().await.is_empty());
}

#[tokio::test]
async fn invariant_violation_dominates_concurrent_infrastructure_failure() {
    let commits = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(CapturingResultPublisher::default());
    let jobs = Arc::new(CapturingJobPublisher::succeeds());
    let dlq = Arc::new(CapturingDlqPublisher::default());
    let mut job = valid_job("mixed-invariant-and-infrastructure");
    job.load.scenarios_per_sec = 100.0;
    job.load.max_concurrency = 2;
    job.load.startup_burst = 2;
    let node = PulseNode::new(
        PulseNodeDependencies {
            elector: Arc::new(FollowerElector),
            due_store: Arc::new(MemoryDispatchStore::default()),
            job_publisher: jobs.clone(),
            job_consumer: Arc::new(QueueJobConsumer::new([TestJobMessage::valid(
                job,
                commits.clone(),
            )])),
            idempotency_store: Arc::new(MemoryExecutionLeaseStore::default()),
            result_publisher: results.clone(),
            dlq_publisher: dlq.clone(),
        },
        vec![build_plan(Arc::new(
            MixedInvariantAndInfrastructureStep::new(),
        ))],
        runtime_config(),
    );
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);

    timeout(Duration::from_secs(2), node.run(shutdown_rx))
        .await
        .expect("mixed invariant and infrastructure failure did not fail-stop the node");

    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(jobs.jobs.lock().await.is_empty());
    assert!(results.results.lock().await.is_empty());
    assert!(dlq.failed_jobs.lock().await.is_empty());
    assert!(dlq.poison_records.lock().await.is_empty());
}
