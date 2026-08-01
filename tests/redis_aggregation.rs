use std::time::Duration;

use pulse::application::aggregation::{
    RunAggregationStore, RunAggregationStoreError, RunAggregationUpdate, RunExpiryOutcome,
    RunFinalizationOutcome, SummaryAcknowledgement,
};
use pulse::domain::contracts::{
    CURRENT_CONTRACT_VERSION, ErrorCount, JobSlice, LatencyBucket, MAX_CONTRACT_EXACT_INTEGER,
    ScenarioRunResult, ScenarioRunStatus, ScenarioRunSummaryStatus, build_terminal_event_id,
};
use pulse::domain::coordination::{DispatchStore, DispatchWindow};
use pulse::domain::error::ContractError;
use pulse::infrastructure::redis::{RedisDueStateStore, RedisRunAggregationStore};
use redis::{AsyncCommands, Client};
use uuid::Uuid;

fn test_redis_url() -> String {
    std::env::var("PULSE_TEST_REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string())
}

fn unique_prefix() -> String {
    format!("pulse:test:aggregate:{}", Uuid::new_v4().simple())
}

async fn redis_time_ms(client: &Client) -> u128 {
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("Redis is reachable");
    let (seconds, microseconds): (u64, u64) = redis::cmd("TIME")
        .query_async(&mut connection)
        .await
        .expect("Redis TIME is readable");
    u128::from(seconds) * 1_000 + u128::from(microseconds / 1_000)
}

fn result(
    index: u32,
    total_slices: u32,
    successes: u64,
    low_latency_count: u64,
) -> ScenarioRunResult {
    let execution_key = format!("run-1:slice-{index}-of-{total_slices}");
    ScenarioRunResult {
        schema_version: CURRENT_CONTRACT_VERSION,
        scenario_id: "checkout".to_string(),
        run_id: "run-1".to_string(),
        event_id: build_terminal_event_id(&execution_key, 0, "result"),
        attempt: 0,
        execution_key,
        slice: JobSlice {
            index,
            total: total_slices,
        },
        started_at_unix_ms: 1,
        finished_at_unix_ms: 2,
        status: ScenarioRunStatus::Success,
        total: successes,
        success: successes,
        failure: 0,
        scenario_latency_p50_ms: 10,
        scenario_latency_p95_ms: 100,
        scenario_latency_p99_ms: 100,
        latency_histogram: vec![
            LatencyBucket {
                upper_bound_ms: 10,
                count: low_latency_count,
            },
            LatencyBucket {
                upper_bound_ms: 100,
                count: successes - low_latency_count,
            },
        ],
        error_breakdown: Vec::new(),
    }
}

fn store(client: Client, prefix: String, max_error_kinds: usize) -> RedisRunAggregationStore {
    RedisRunAggregationStore::new(
        client,
        prefix,
        Duration::from_millis(100),
        Duration::from_secs(30),
    )
    .expect("valid aggregation configuration")
    .with_operation_timeout(Duration::from_secs(1))
    .expect("valid operation timeout")
    .with_max_error_kinds(max_error_kinds)
    .expect("valid error-kind capacity")
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn duplicate_and_out_of_order_slices_complete_exactly_once() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let prefix = unique_prefix();
    let store = store(client.clone(), prefix.clone(), 8);
    let second = result(1, 2, 3, 0);

    assert!(matches!(
        store.ingest(&second, 100).await.expect("slice is accepted"),
        RunAggregationUpdate::Accepted {
            received_slices: 1,
            expected_slices: 2,
        }
    ));
    assert!(matches!(
        store
            .ingest(&second, 101)
            .await
            .expect("duplicate is a typed outcome"),
        RunAggregationUpdate::Duplicate {
            received_slices: 1,
            expected_slices: 2,
            finalized_status: None,
        }
    ));

    let completed = store
        .ingest(&result(0, 2, 7, 7), 102)
        .await
        .expect("missing slice completes the run");
    let RunAggregationUpdate::Completed(summary) = completed else {
        panic!("expected a complete summary, got {completed:?}");
    };
    assert_eq!(summary.status, ScenarioRunSummaryStatus::Complete);
    assert_eq!(summary.total, 10);
    assert_eq!(summary.success, 10);
    assert_eq!(summary.received_slices, 2);
    assert!(summary.missing_slices.is_empty());
    assert_eq!(summary.scenario_latency_p50_ms, 10);
    assert_eq!(summary.scenario_latency_p95_ms, 100);

    let pending = store
        .pending_summaries(10)
        .await
        .expect("complete summary is discoverable after restart");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].revision, 1);
    assert_eq!(pending[0].event_id, "run-1:summary:r1:complete");
    assert!(pending[0].pending_publication);
    assert_eq!(
        store
            .acknowledge_summary("run-1", 1)
            .await
            .expect("summary acknowledgement is persisted"),
        SummaryAcknowledgement::Acknowledged
    );
    assert!(store.pending_summaries(10).await.unwrap().is_empty());
    let loaded = store
        .load_summary("run-1")
        .await
        .expect("summary remains retained")
        .expect("summary exists");
    assert!(!loaded.pending_publication);
    assert_eq!(
        store.acknowledge_summary("run-1", 1).await.unwrap(),
        SummaryAcknowledgement::AlreadyAcknowledged
    );

    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("Redis is reachable");
    let ttl_ms: i64 = connection
        .pttl(format!("{prefix}:{{runs}}:run:run-1"))
        .await
        .expect("aggregation key has a TTL");
    assert!(ttl_ms > 0 && ttl_ms <= 30_000);
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn pending_outbox_prevents_expiry_until_acknowledgement_starts_retention() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let prefix = unique_prefix();
    let store = store(client.clone(), prefix.clone(), 8);

    let completed = store
        .ingest(&result(0, 1, 1, 1), 100)
        .await
        .expect("single slice completes the run");
    assert!(matches!(completed, RunAggregationUpdate::Completed(_)));

    let run_key = format!("{prefix}:{{runs}}:run:run-1");
    let outbox_key = format!("{prefix}:{{runs}}:summary-outbox");
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("Redis is reachable");
    let pending_ttl_ms: i64 = connection
        .pttl(&run_key)
        .await
        .expect("pending run PTTL is readable");
    let pending_rank: Option<usize> = connection
        .zrank(&outbox_key, "run-1")
        .await
        .expect("pending run is indexed in the outbox");
    assert_eq!(
        pending_ttl_ms, -1,
        "an unacknowledged outbox entry must not expire"
    );
    assert_eq!(pending_rank, Some(0));

    assert_eq!(
        store
            .acknowledge_summary("run-1", 1)
            .await
            .expect("summary acknowledgement is persisted"),
        SummaryAcknowledgement::Acknowledged
    );
    let acknowledged_ttl_ms: i64 = connection
        .pttl(&run_key)
        .await
        .expect("acknowledged run PTTL is readable");
    let acknowledged_rank: Option<usize> = connection
        .zrank(&outbox_key, "run-1")
        .await
        .expect("settled outbox index is readable");
    assert!(
        acknowledged_ttl_ms > 0 && acknowledged_ttl_ms <= 30_000,
        "acknowledgement must start the configured retention TTL, got {acknowledged_ttl_ms}ms"
    );
    assert_eq!(acknowledged_rank, None);
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn redis_time_is_authoritative_for_first_result_deadline_and_expiry() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let prefix = unique_prefix();
    let store = store(client.clone(), prefix.clone(), 8);
    let before = redis_time_ms(&client).await;

    store
        .ingest(&result(0, 2, 1, 1), u128::MAX)
        .await
        .expect("application clock is not the durable aggregation clock");

    let after = redis_time_ms(&client).await;
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("Redis is reachable");
    let (first_result_at, deadline_at): (u64, u64) = redis::pipe()
        .cmd("HGET")
        .arg(format!("{prefix}:{{runs}}:run:run-1"))
        .arg("first_result_at")
        .cmd("HGET")
        .arg(format!("{prefix}:{{runs}}:run:run-1"))
        .arg("deadline_at")
        .query_async(&mut connection)
        .await
        .expect("aggregation timestamps are readable");
    assert!(u128::from(first_result_at) >= before && u128::from(first_result_at) <= after);
    assert_eq!(deadline_at, first_result_at + 100);
    assert!(
        store.due_runs(u128::MAX, 10).await.unwrap().is_empty(),
        "a fast application clock must not scan a run before Redis's deadline"
    );
    assert!(matches!(
        store.mark_expired("run-1", u128::MAX).await.unwrap(),
        RunExpiryOutcome::NotExpired { .. }
    ));

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(store.due_runs(0, 10).await.unwrap(), vec!["run-1"]);
    assert!(matches!(
        store.mark_expired("run-1", 0).await.unwrap(),
        RunExpiryOutcome::MarkedTimedOut(_)
    ));
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn registered_run_with_zero_results_becomes_a_durable_timed_out_summary() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let prefix = unique_prefix();
    let registration = RedisDueStateStore::new(
        client.clone(),
        format!(
            "pulse:{{coordination}}:schedule:{}",
            Uuid::new_v4().simple()
        ),
    )
    .with_aggregation_registration(
        prefix.clone(),
        Duration::from_millis(100),
        Duration::from_secs(30),
        8,
    );
    let store = store(client.clone(), prefix.clone(), 8);
    let scheduled_at = redis_time_ms(&client).await;
    let window = DispatchWindow {
        scenario_id: "checkout".to_string(),
        window_id: "zero-result-run".to_string(),
        run_id: "zero-result-run".to_string(),
        scheduled_at_unix_ms: scheduled_at,
        contract_version: CURRENT_CONTRACT_VERSION,
        total_slices: 2,
        plan_fingerprint: "plan-v1".to_string(),
        missing_slices: vec![0, 1],
    };

    registration
        .register_run(&window, Duration::from_secs(1))
        .await
        .expect("scheduler registration is durable before publication");
    assert!(
        store.due_runs(u128::MAX, 10).await.unwrap().is_empty(),
        "a fast application clock must not make a registered run due"
    );
    tokio::time::sleep(Duration::from_millis(1_150)).await;
    assert_eq!(
        store.due_runs(0, 10).await.unwrap(),
        vec!["zero-result-run"]
    );

    let expired = store
        .mark_expired("zero-result-run", 0)
        .await
        .expect("registered zero-result run is discoverable at its deadline");
    let RunExpiryOutcome::MarkedTimedOut(summary) = expired else {
        panic!("expected timed-out summary, got {expired:?}");
    };
    assert_eq!(summary.status, ScenarioRunSummaryStatus::TimedOut);
    assert_eq!(summary.expected_slices, 2);
    assert_eq!(summary.received_slices, 0);
    assert_eq!(summary.missing_slices, vec![0, 1]);
    assert_eq!(summary.total, 0);
    assert_eq!(summary.success, 0);
    assert_eq!(summary.failure, 0);
    assert!(summary.latency_histogram.is_empty());

    let pending = store
        .pending_summaries(10)
        .await
        .expect("zero-result timeout is durably queued for publication");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].summary.run_id, "zero-result-run");
    assert_eq!(pending[0].event_id, "zero-result-run:summary:r1:timed-out");
    assert!(pending[0].pending_publication);

    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .expect("Redis is reachable");
    let ttl_ms: i64 = connection
        .pttl(format!("{prefix}:{{runs}}:run:zero-result-run"))
        .await
        .expect("pending zero-result summary PTTL is readable");
    assert_eq!(
        ttl_ms, -1,
        "pending zero-result summary must survive restart"
    );
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn timed_out_run_accepts_late_slices_and_becomes_complete() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let store = store(client, unique_prefix(), 8);
    let first = result(0, 2, 2, 2);
    store
        .ingest(&first, 100)
        .await
        .expect("first slice accepted");

    assert!(store.due_runs(u128::MAX, 10).await.unwrap().is_empty());

    assert!(matches!(
        store
            .mark_expired("run-1", u128::MAX)
            .await
            .expect("not-yet-expired outcome"),
        RunExpiryOutcome::NotExpired { retry_after }
            if retry_after > Duration::ZERO && retry_after <= Duration::from_millis(100)
    ));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(store.due_runs(0, 10).await.unwrap(), vec!["run-1"]);
    let timed_out = store
        .mark_expired("run-1", 0)
        .await
        .expect("expired run is marked partial");
    let RunExpiryOutcome::MarkedTimedOut(summary) = timed_out else {
        panic!("expected timed-out summary, got {timed_out:?}");
    };
    assert_eq!(summary.status, ScenarioRunSummaryStatus::TimedOut);
    assert_eq!(summary.missing_slices, vec![1]);
    assert!(store.due_runs(u128::MAX, 10).await.unwrap().is_empty());
    let timed_out_revision = store
        .load_summary("run-1")
        .await
        .unwrap()
        .expect("timed-out summary exists");
    assert_eq!(timed_out_revision.revision, 1);
    assert_eq!(timed_out_revision.event_id, "run-1:summary:r1:timed-out");
    assert_eq!(
        store.acknowledge_summary("run-1", 1).await.unwrap(),
        SummaryAcknowledgement::Acknowledged
    );

    assert!(matches!(
        store
            .ingest(&first, 210)
            .await
            .expect("late duplicate is harmless"),
        RunAggregationUpdate::Duplicate {
            finalized_status: Some(ScenarioRunSummaryStatus::TimedOut),
            ..
        }
    ));
    let late = store
        .ingest(&result(1, 2, 1, 0), 220)
        .await
        .expect("late missing slice is retained");
    let RunAggregationUpdate::LateCompleted(summary) = late else {
        panic!("expected late completion, got {late:?}");
    };
    assert_eq!(summary.status, ScenarioRunSummaryStatus::Complete);
    assert_eq!(summary.total, 3);
    assert!(summary.missing_slices.is_empty());
    let completed_revision = store
        .load_summary("run-1")
        .await
        .unwrap()
        .expect("late complete summary exists");
    assert_eq!(completed_revision.revision, 2);
    assert_eq!(completed_revision.event_id, "run-1:summary:r2:complete");
    assert!(completed_revision.pending_publication);
    assert_eq!(
        store.acknowledge_summary("run-1", 1).await.unwrap(),
        SummaryAcknowledgement::Stale {
            current_revision: 2
        }
    );
    assert_eq!(store.pending_summaries(10).await.unwrap().len(), 1);
    assert_eq!(
        store.acknowledge_summary("run-1", 2).await.unwrap(),
        SummaryAcknowledgement::Acknowledged
    );
    assert!(store.pending_summaries(10).await.unwrap().is_empty());

    assert!(matches!(
        store
            .mark_expired("run-1", 300)
            .await
            .expect("complete state remains readable"),
        RunExpiryOutcome::AlreadyFinalized(ref summary)
            if summary.status == ScenarioRunSummaryStatus::Complete
    ));
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn inconsistent_metadata_and_error_cardinality_do_not_mutate_run() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let store = store(client, unique_prefix(), 1);
    let mut first = result(0, 2, 1, 1);
    first.status = ScenarioRunStatus::Failed;
    first.success = 0;
    first.failure = 1;
    first.error_breakdown = vec![ErrorCount {
        kind: "target_status".to_string(),
        count: 1,
    }];
    store
        .ingest(&first, 100)
        .await
        .expect("first slice accepted");

    let mut inconsistent = result(1, 2, 1, 1);
    inconsistent.scenario_id = "different-scenario".to_string();
    assert!(matches!(
        store.ingest(&inconsistent, 110).await,
        Err(RunAggregationStoreError::InconsistentResult { .. })
    ));

    let mut excess_kind = result(1, 2, 1, 1);
    excess_kind.status = ScenarioRunStatus::Failed;
    excess_kind.success = 0;
    excess_kind.failure = 1;
    excess_kind.error_breakdown = vec![ErrorCount {
        kind: "request_timeout".to_string(),
        count: 1,
    }];
    assert!(matches!(
        store.ingest(&excess_kind, 120).await,
        Err(RunAggregationStoreError::ErrorKindCapacity { max_error_kinds: 1 })
    ));

    let partial = store
        .finalize_run("run-1", ScenarioRunSummaryStatus::Partial, 150)
        .await
        .expect("unchanged run can be explicitly finalized partial");
    let RunFinalizationOutcome::Finalized(summary) = partial else {
        panic!("expected partial summary, got {partial:?}");
    };
    assert_eq!(summary.received_slices, 1);
    assert_eq!(summary.total, 1);
    assert_eq!(summary.missing_slices, vec![1]);
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn explicit_cancellation_is_durable_and_rejects_unseen_slices() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let store = store(client, unique_prefix(), 8);
    let first = result(0, 2, 1, 1);
    store
        .ingest(&first, 100)
        .await
        .expect("first slice accepted");

    let finalized = store
        .finalize_run("run-1", ScenarioRunSummaryStatus::Cancelled, 120)
        .await
        .expect("explicit cancellation is persisted");
    let RunFinalizationOutcome::Finalized(summary) = finalized else {
        panic!("expected cancelled summary, got {finalized:?}");
    };
    assert_eq!(summary.status, ScenarioRunSummaryStatus::Cancelled);
    assert_eq!(summary.missing_slices, vec![1]);
    let durable = store
        .load_summary("run-1")
        .await
        .unwrap()
        .expect("cancelled summary exists");
    assert_eq!(durable.event_id, "run-1:summary:r1:cancelled");

    assert!(matches!(
        store.ingest(&first, 130).await.unwrap(),
        RunAggregationUpdate::Duplicate {
            finalized_status: Some(ScenarioRunSummaryStatus::Cancelled),
            ..
        }
    ));
    assert!(matches!(
        store.ingest(&result(1, 2, 1, 1), 140).await,
        Err(RunAggregationStoreError::InconsistentResult { .. })
    ));
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn active_run_capacity_applies_backpressure_without_creating_partial_state() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let store = store(client, unique_prefix(), 8)
        .with_max_active_runs(1)
        .expect("valid active-run bound");
    store
        .ingest(&result(0, 2, 1, 1), 100)
        .await
        .expect("first run occupies bounded capacity");

    let mut other = result(0, 2, 1, 1);
    other.run_id = "run-2".to_string();
    other.execution_key = "run-2:slice-0-of-2".to_string();
    other.event_id = build_terminal_event_id(&other.execution_key, 0, "result");
    assert!(matches!(
        store.ingest(&other, 110).await,
        Err(RunAggregationStoreError::ActiveRunCapacity { max_active_runs: 1 })
    ));
    assert!(store.load_summary("run-2").await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires an intentionally unavailable Redis endpoint"]
async fn redis_outage_is_never_reported_as_duplicate_or_accepted() {
    let client = Client::open("redis://127.0.0.1:1").expect("valid unavailable Redis URL");
    let store = RedisRunAggregationStore::new(
        client,
        unique_prefix(),
        Duration::from_secs(1),
        Duration::from_secs(30),
    )
    .expect("valid aggregation configuration")
    .with_operation_timeout(Duration::from_millis(100))
    .expect("valid operation timeout");

    assert!(matches!(
        store.ingest(&result(0, 2, 1, 1), 100).await,
        Err(RunAggregationStoreError::Unavailable { .. } | RunAggregationStoreError::Timeout { .. })
    ));
}

#[tokio::test]
#[ignore = "requires docker compose Redis"]
async fn aggregate_exact_integer_overflow_is_a_permanent_contract_outcome() {
    let client = Client::open(test_redis_url()).expect("valid Redis URL");
    let store = store(client, unique_prefix(), 8);
    let mut first = result(0, 2, MAX_CONTRACT_EXACT_INTEGER, 0);
    first.latency_histogram[1].count = MAX_CONTRACT_EXACT_INTEGER;
    store
        .ingest(&first, 100)
        .await
        .expect("first exact-range slice is accepted");

    let overflow = store.ingest(&result(1, 2, 1, 1), 101).await;
    assert!(matches!(
        overflow,
        Err(RunAggregationStoreError::Contract(
            ContractError::InvalidResult(_)
        ))
    ));
}
