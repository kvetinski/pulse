# Architecture Decisions

This document captures key runtime decisions in Pulse and the tradeoffs behind them.

## 1. Scheduler/Worker Split

Decision:
- Every Pulse pod runs both a scheduler loop and a worker loop.
- Scheduling is active only on the leader pod.
- Workers are active on all pods and consume jobs from Kafka.

Why:
- Keeps pod roles uniform (same binary, same deployment).
- Allows horizontal scaling by adding worker capacity with more replicas.
- Avoids a separate scheduler service deployment.

How it works:
- Scheduler loop: checks due scenarios, computes slices, publishes jobs.
- Worker loop: consumes jobs, executes scenario slices, publishes results.
- Source: `src/application/service.rs`.

Tradeoffs:
- Simpler operations model than separate scheduler/worker deployments.
- Slightly more resource usage per pod because both loops are always present.

## 2. Leader Election

Decision:
- Use Redis lock for leader election with acquire/renew semantics.
- Leader ownership is keyed by `PULSE_REDIS_LEADER_KEY`.
- Lock is periodically renewed; loss of renew means leadership loss.

Why:
- Reuses existing Redis dependency.
- Provides a lightweight single-active-scheduler mechanism.

How it works:
- Acquire: `SET key value NX PX ttl`.
- Renew: Lua script verifies owner then updates TTL (`PEXPIRE`).
- Relinquish: Lua script deletes lock only if current node owns it.
- Source: `src/infrastructure/redis/mod.rs`.

Tradeoffs:
- Simple and effective for single-cluster deployments.
- Not a consensus protocol; behavior depends on Redis availability and timing.
- Short TTL + renew interval tuning is required to avoid flapping.

## 3. Idempotency

Decision:
- Use Redis idempotency keys to ensure each `execution_key + attempt` is processed once.
- Workers claim idempotency before execution.

Why:
- Kafka delivery can produce duplicates (rebalances/retries).
- Exactly-once execution of scenario jobs is required at application level.

How it works:
- `SETNX` on `PULSE_REDIS_IDEMPOTENCY_PREFIX:<execution_key>:attempt-<n>`.
- On success, set TTL; on failure, treat as duplicate and skip execution.
- Source: `src/infrastructure/redis/mod.rs`, `worker_loop` in `src/application/service.rs`.

Tradeoffs:
- Strong duplicate suppression for the TTL window.
- If TTL is too short, very late duplicates may execute again.
- Requires Redis availability for duplicate protection.

## 4. Partition Strategy

Decision:
- Job key is configurable:
  - `execution_key` (default)
  - `scenario_id`

Why:
- Different ordering/distribution behavior is needed for different workloads.

How it works:
- Scheduler computes `ScenarioJob.execution_key` and passes partition key to Kafka producer.
- `execution_key` includes scenario, schedule window, and slice index.
- Source: `src/domain/contracts.rs`, `scheduler_loop` in `src/application/service.rs`.

Operational meaning:
- `execution_key`:
  - Better spread across partitions for high fan-out runs.
  - Ordering is per execution slice key.
- `scenario_id`:
  - All runs of one scenario hash to the same key/partition.
  - More ordering locality, less parallel partition spread.

Tradeoffs:
- `execution_key` maximizes throughput distribution.
- `scenario_id` favors ordering locality and simpler traceability.

## 5. Backpressure Behavior

Decision:
- Backpressure is applied at multiple layers:
  - Scenario start rate via token bucket (`scenarios_per_sec`).
  - In-flight cap via semaphore (`max_concurrency`).
  - Kafka consumer pull loop (work pulled only as fast as worker processes jobs).

Why:
- Prevent runaway concurrency and protect target services.
- Keep load shape stable over time windows.

How it works:
- Runner token bucket gates scenario starts (`TokenBucket::acquire`).
- Semaphore bounds concurrent scenario executions.
- Scheduler slices high-load scenarios into smaller jobs.
- Worker consumes one job, runs it, publishes result, commits offset.
- Sources: `src/application/rate_limiter.rs`, `src/application/runner.rs`, `src/application/service.rs`.

Important behavior:
- Configured rate is scenarios/sec, not requests/sec.
- Effective throughput is bounded by:
  - configured `scenarios_per_sec`,
  - `max_concurrency`,
  - scenario latency and downstream service capacity.
- If target systems slow down, in-flight executions fill semaphore slots, and start rate naturally degrades.

Known limits:
- Token bucket currently has a 1 scenario/sec minimum floor per slice.
- This is why scheduler slice count is bounded by scenario rate.
- See `calculate_slices` in `src/application/service.rs`.

## 6. Retry and Dead-Letter

Decision:
- Retries are worker-level, job-based retries with exponential backoff.
- Failed jobs are published to a dedicated dead-letter topic after retry exhaustion or retry publish failure.

Why:
- Keeps scenario scheduling simple while improving resilience to transient failures.
- Makes unrecoverable failures explicit and observable via DLQ.

How it works:
- Scheduler publishes jobs with retry metadata (`attempt=0`, `max_retries=<configured>`).
- Worker retries on failure when `attempt < max_retries`.
- Delay is exponential from `PULSE_WORKER_RETRY_BASE_DELAY_MS`, capped.
- Final failures are published as `FailedScenarioJob` to `PULSE_KAFKA_DLQ_TOPIC`.
- Source: `src/application/service.rs`, `src/domain/contracts.rs`, `src/infrastructure/kafka/mod.rs`.

Tradeoffs:
- Retries are not per-step yet (only per scenario job).
- Repeated failures can increase total system load; tuning is required.

## Non-goals (Current Phase)

- No exactly-once guarantee across infinite time (TTL-bounded idempotency).
- No cross-region consensus leader election.
- No explicit global queue length based throttling yet.
