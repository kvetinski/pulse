use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::scenario::RepeatPolicy;

/// Coordination failures are deliberately separate from legitimate contention.
/// Callers must never interpret an error as a duplicate, a follower state, or a
/// completed dispatch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoordinationError {
    Unavailable {
        operation: &'static str,
        message: String,
    },
    Timeout {
        operation: &'static str,
    },
    InvalidState {
        operation: &'static str,
        message: String,
    },
    StaleOwner {
        operation: &'static str,
    },
}

impl CoordinationError {
    pub fn unavailable(operation: &'static str, message: impl Into<String>) -> Self {
        Self::Unavailable {
            operation,
            message: message.into(),
        }
    }

    pub fn invalid_state(operation: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidState {
            operation,
            message: message.into(),
        }
    }

    pub fn stale_owner(operation: &'static str) -> Self {
        Self::StaleOwner { operation }
    }
}

impl Display for CoordinationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { operation, message } => {
                write!(
                    f,
                    "coordination operation '{operation}' is unavailable: {message}"
                )
            }
            Self::Timeout { operation } => {
                write!(f, "coordination operation '{operation}' timed out")
            }
            Self::InvalidState { operation, message } => {
                write!(
                    f,
                    "coordination operation '{operation}' found invalid state: {message}"
                )
            }
            Self::StaleOwner { operation } => {
                write!(
                    f,
                    "coordination operation '{operation}' rejected a stale owner"
                )
            }
        }
    }
}

impl std::error::Error for CoordinationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LeaderLease {
    pub lock_key: String,
    pub node_id: String,
    pub owner_token: String,
    pub fencing_token: u64,
    pub expires_at_unix_ms: u64,
    pub ttl: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LeadershipOutcome {
    Acquired(LeaderLease),
    Renewed(LeaderLease),
    Follower { retry_after: Duration },
}

#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// Acquire a new lease when `current` is `None`, otherwise renew exactly the
    /// supplied lease. A failed renewal never silently acquires a replacement.
    async fn acquire_or_renew(
        &self,
        current: Option<&LeaderLease>,
    ) -> Result<LeadershipOutcome, CoordinationError>;

    async fn relinquish(&self, lease: &LeaderLease) -> Result<(), CoordinationError>;
}

#[derive(Clone, Debug)]
pub struct DispatchSpec {
    pub scenario_id: String,
    pub contract_version: u16,
    pub total_slices: u32,
    pub repeat: RepeatPolicy,
    pub plan_fingerprint: String,
}

impl DispatchSpec {
    pub fn new(
        scenario_id: impl Into<String>,
        contract_version: u16,
        total_slices: u32,
        repeat: RepeatPolicy,
        plan_fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            scenario_id: scenario_id.into(),
            contract_version,
            total_slices,
            repeat,
            plan_fingerprint: plan_fingerprint.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchWindow {
    pub scenario_id: String,
    pub window_id: String,
    pub run_id: String,
    pub scheduled_at_unix_ms: u128,
    pub contract_version: u16,
    pub total_slices: u32,
    pub plan_fingerprint: String,
    pub missing_slices: Vec<u32>,
}

impl DispatchWindow {
    /// Returns the deterministic identity used for a slice in this schedule
    /// window. It is stable across publication retries and leader changes.
    pub fn execution_key(&self, slice_index: u32) -> Option<String> {
        (slice_index < self.total_slices).then(|| {
            format!(
                "{}:slice-{}-of-{}",
                self.window_id, slice_index, self.total_slices
            )
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Ready(DispatchWindow),
    NotDue { retry_after: Duration },
    Finished,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchProgress {
    Pending { remaining_slices: u32 },
    Complete,
}

#[async_trait]
pub trait DispatchStore: Send + Sync {
    async fn prepare_window(
        &self,
        spec: &DispatchSpec,
        leader: &LeaderLease,
    ) -> Result<DispatchOutcome, CoordinationError>;

    async fn ack_slice(
        &self,
        window: &DispatchWindow,
        slice_index: u32,
        leader: &LeaderLease,
    ) -> Result<DispatchProgress, CoordinationError>;

    /// Persist expected run metadata before the first slice is published. The
    /// default keeps lightweight/fake dispatch stores backwards compatible;
    /// production Redis dispatch overrides it with a durable registration.
    async fn register_run(
        &self,
        _window: &DispatchWindow,
        _load_duration: Duration,
    ) -> Result<(), CoordinationError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOutcome {
    ResultPublished,
    RetryPublished,
    DeadLetterPublished,
}

impl TerminalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResultPublished => "completed",
            Self::RetryPublished => "retry_published",
            Self::DeadLetterPublished => "dlq_published",
        }
    }
}

impl FromStr for TerminalOutcome {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "completed" => Ok(Self::ResultPublished),
            "retry_published" => Ok(Self::RetryPublished),
            "dlq_published" => Ok(Self::DeadLetterPublished),
            _ => Err(format!("unknown terminal outcome '{value}'")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedOutcome {
    pub outcome: TerminalOutcome,
    pub completed_at_unix_ms: u128,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionClaim {
    pub execution_key: String,
    pub attempt: u32,
}

impl ExecutionClaim {
    pub fn new(execution_key: impl Into<String>, attempt: u32) -> Self {
        Self {
            execution_key: execution_key.into(),
            attempt,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionLease {
    pub execution_key: String,
    pub attempt: u32,
    pub owner_token: String,
    pub expires_at_unix_ms: u128,
    pub ttl: Duration,
    pub recovered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Acquired(ExecutionLease),
    AlreadyCompleted(CompletedOutcome),
    Busy { retry_after: Duration },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    Released,
    AlreadyAbsent,
}

#[async_trait]
pub trait ExecutionLeaseStore: Send + Sync {
    async fn claim(&self, claim: &ExecutionClaim) -> Result<ClaimOutcome, CoordinationError>;

    async fn renew(&self, lease: &ExecutionLease) -> Result<ExecutionLease, CoordinationError>;

    async fn complete(
        &self,
        lease: &ExecutionLease,
        outcome: TerminalOutcome,
    ) -> Result<CompletedOutcome, CoordinationError>;

    async fn release(&self, lease: &ExecutionLease) -> Result<ReleaseOutcome, CoordinationError>;
}

#[cfg(test)]
mod tests {
    use super::{DispatchWindow, TerminalOutcome};

    #[test]
    fn dispatch_execution_identity_is_deterministic_and_bounded() {
        let window = DispatchWindow {
            scenario_id: "Checkout".to_string(),
            window_id: "v2:s8:Checkout:w123:n2".to_string(),
            run_id: "v2:s8:Checkout:w123:n2".to_string(),
            scheduled_at_unix_ms: 123,
            contract_version: 2,
            total_slices: 2,
            plan_fingerprint: "sha256:plan".to_string(),
            missing_slices: vec![0, 1],
        };

        assert_eq!(
            window.execution_key(1).as_deref(),
            Some("v2:s8:Checkout:w123:n2:slice-1-of-2")
        );
        assert_eq!(window.execution_key(1), window.execution_key(1));
        assert_eq!(window.execution_key(2), None);
    }

    #[test]
    fn terminal_outcome_storage_names_round_trip() {
        for outcome in [
            TerminalOutcome::ResultPublished,
            TerminalOutcome::RetryPublished,
            TerminalOutcome::DeadLetterPublished,
        ] {
            assert_eq!(outcome.as_str().parse::<TerminalOutcome>(), Ok(outcome));
        }
        assert!("future_terminal_state".parse::<TerminalOutcome>().is_err());
    }
}
