use std::time::Duration;

use pulse::domain::coordination::{
    ClaimOutcome, CoordinationError, DispatchOutcome, DispatchProgress, DispatchSpec,
    DispatchStore, ExecutionClaim, ExecutionLease, ExecutionLeaseStore, LeaderElector, LeaderLease,
    LeadershipOutcome, TerminalOutcome,
};
use pulse::domain::scenario::RepeatPolicy;
use pulse::infrastructure::redis::{RedisDueStateStore, RedisIdempotencyStore, RedisLeaderElector};
use redis::Client;
use tokio::time::sleep;
use uuid::Uuid;

fn test_redis_url() -> String {
    std::env::var("PULSE_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string())
}

fn unique_key(kind: &str) -> String {
    format!("pulse:test:{kind}:{}", Uuid::new_v4().simple())
}

fn acquired_leader(outcome: LeadershipOutcome) -> LeaderLease {
    match outcome {
        LeadershipOutcome::Acquired(lease) => lease,
        other => panic!("expected acquired leader lease, got {other:?}"),
    }
}

fn acquired_execution(outcome: ClaimOutcome) -> ExecutionLease {
    match outcome {
        ClaimOutcome::Acquired(lease) => lease,
        other => panic!("expected acquired execution lease, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn execution_lease_recovers_and_rejects_stale_owner() {
    let client = Client::open(test_redis_url()).expect("valid Redis test URL");
    let store = RedisIdempotencyStore::with_timings(
        client,
        unique_key("execution"),
        Duration::from_millis(150),
        Duration::from_secs(10),
        Duration::from_secs(1),
    );
    let claim = ExecutionClaim::new("scenario:window:slice-0-of-1", 0);

    let first = acquired_execution(store.claim(&claim).await.expect("first claim succeeds"));
    assert!(!first.recovered);
    assert!(matches!(
        store
            .claim(&claim)
            .await
            .expect("contention is not an error"),
        ClaimOutcome::Busy { .. }
    ));

    sleep(Duration::from_millis(225)).await;
    let recovered = acquired_execution(
        store
            .claim(&claim)
            .await
            .expect("expired claim is recovered"),
    );
    assert!(recovered.recovered);
    assert_ne!(first.owner_token, recovered.owner_token);

    assert!(matches!(
        store.renew(&first).await,
        Err(CoordinationError::StaleOwner { .. })
    ));
    assert!(matches!(
        store
            .complete(&first, TerminalOutcome::ResultPublished)
            .await,
        Err(CoordinationError::StaleOwner { .. })
    ));

    let recovered = store.renew(&recovered).await.expect("current owner renews");
    let completed = store
        .complete(&recovered, TerminalOutcome::ResultPublished)
        .await
        .expect("current owner completes");
    assert_eq!(completed.outcome, TerminalOutcome::ResultPublished);

    assert!(matches!(
        store.claim(&claim).await.expect("terminal claim is readable"),
        ClaimOutcome::AlreadyCompleted(ref outcome)
            if outcome.outcome == TerminalOutcome::ResultPublished
    ));
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn leader_fence_increases_and_stale_lease_cannot_mutate_dispatch() {
    let client = Client::open(test_redis_url()).expect("valid Redis test URL");
    let lock_key = unique_key("leader");
    let elector = RedisLeaderElector::new(
        client.clone(),
        lock_key,
        "redis-coordination-test".to_string(),
        3_000,
    )
    .with_operation_timeout(Duration::from_secs(1));
    let dispatch = RedisDueStateStore::new(client, unique_key("schedule"))
        .with_operation_timeout(Duration::from_secs(1));

    let first = acquired_leader(
        elector
            .acquire_or_renew(None)
            .await
            .expect("leader acquisition succeeds"),
    );
    let spec = DispatchSpec::new("PartialWindow", 1, 2, RepeatPolicy::Once, "sha256:plan-a");
    let window = match dispatch
        .prepare_window(&spec, &first)
        .await
        .expect("window preparation succeeds")
    {
        DispatchOutcome::Ready(window) => window,
        other => panic!("expected ready window, got {other:?}"),
    };
    assert_eq!(
        dispatch
            .ack_slice(&window, 0, &first)
            .await
            .expect("first slice acknowledgement succeeds"),
        DispatchProgress::Pending {
            remaining_slices: 1
        }
    );

    elector
        .relinquish(&first)
        .await
        .expect("current leader relinquishes");
    let second = acquired_leader(
        elector
            .acquire_or_renew(None)
            .await
            .expect("replacement leader acquisition succeeds"),
    );
    assert!(second.fencing_token > first.fencing_token);
    assert_ne!(second.owner_token, first.owner_token);

    assert!(matches!(
        dispatch.ack_slice(&window, 1, &first).await,
        Err(CoordinationError::StaleOwner { .. })
    ));
    let resumed = match dispatch
        .prepare_window(&spec, &second)
        .await
        .expect("new leader resumes the active window")
    {
        DispatchOutcome::Ready(window) => window,
        other => panic!("expected resumed window, got {other:?}"),
    };
    assert_eq!(resumed.window_id, window.window_id);
    assert_eq!(resumed.missing_slices, vec![1]);
    assert_eq!(
        dispatch
            .ack_slice(&resumed, 1, &second)
            .await
            .expect("replacement leader completes window"),
        DispatchProgress::Complete
    );
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn dispatch_resumes_only_missing_slices_and_rejects_plan_drift() {
    let client = Client::open(test_redis_url()).expect("valid Redis test URL");
    let elector = RedisLeaderElector::new(
        client.clone(),
        unique_key("leader"),
        "dispatch-test".to_string(),
        3_000,
    )
    .with_operation_timeout(Duration::from_secs(1));
    let dispatch = RedisDueStateStore::new(client, unique_key("schedule"))
        .with_operation_timeout(Duration::from_secs(1));
    let leader = acquired_leader(
        elector
            .acquire_or_renew(None)
            .await
            .expect("leader acquisition succeeds"),
    );
    let spec = DispatchSpec::new(
        "RecoverableOnce",
        1,
        3,
        RepeatPolicy::Once,
        "sha256:stable-plan",
    );

    let initial = match dispatch
        .prepare_window(&spec, &leader)
        .await
        .expect("initial window preparation succeeds")
    {
        DispatchOutcome::Ready(window) => window,
        other => panic!("expected ready window, got {other:?}"),
    };
    assert_eq!(initial.missing_slices, vec![0, 1, 2]);
    assert_eq!(initial.run_id, initial.window_id);
    assert_eq!(
        initial.execution_key(2).as_deref(),
        Some(format!("{}:slice-2-of-3", initial.window_id).as_str())
    );
    assert_eq!(
        dispatch
            .ack_slice(&initial, 0, &leader)
            .await
            .expect("first acknowledgement succeeds"),
        DispatchProgress::Pending {
            remaining_slices: 2
        }
    );

    let resumed = match dispatch
        .prepare_window(&spec, &leader)
        .await
        .expect("active window resumes")
    {
        DispatchOutcome::Ready(window) => window,
        other => panic!("expected resumed window, got {other:?}"),
    };
    assert_eq!(resumed.window_id, initial.window_id);
    assert_eq!(resumed.scheduled_at_unix_ms, initial.scheduled_at_unix_ms);
    assert_eq!(resumed.missing_slices, vec![1, 2]);

    let drifted = DispatchSpec::new(
        "RecoverableOnce",
        1,
        3,
        RepeatPolicy::Once,
        "sha256:different-plan",
    );
    assert!(matches!(
        dispatch.prepare_window(&drifted, &leader).await,
        Err(CoordinationError::InvalidState { .. })
    ));

    assert!(matches!(
        dispatch
            .ack_slice(&resumed, 1, &leader)
            .await
            .expect("second acknowledgement succeeds"),
        DispatchProgress::Pending {
            remaining_slices: 1
        }
    ));
    assert_eq!(
        dispatch
            .ack_slice(&resumed, 2, &leader)
            .await
            .expect("final acknowledgement succeeds"),
        DispatchProgress::Complete
    );
    assert_eq!(
        dispatch
            .ack_slice(&resumed, 2, &leader)
            .await
            .expect("final acknowledgement is idempotent"),
        DispatchProgress::Complete
    );
    assert_eq!(
        dispatch
            .prepare_window(&spec, &leader)
            .await
            .expect("completed once window is readable"),
        DispatchOutcome::Finished
    );
}

#[tokio::test]
#[ignore = "requires an intentionally unavailable Redis endpoint"]
async fn redis_unavailability_is_an_error_not_a_duplicate() {
    let client = Client::open("redis://127.0.0.1:1").expect("valid unavailable Redis URL");
    let store = RedisIdempotencyStore::with_timings(
        client,
        unique_key("unavailable"),
        Duration::from_secs(1),
        Duration::from_secs(10),
        Duration::from_millis(100),
    );
    let claim = ExecutionClaim::new("unavailable-test", 0);

    assert!(matches!(
        store.claim(&claim).await,
        Err(CoordinationError::Unavailable { .. } | CoordinationError::Timeout { .. })
    ));
}
