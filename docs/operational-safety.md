# Operational Safety

This document describes runtime safety behavior for Pulse in production-like environments.

## Graceful Shutdown

Pulse listens for:
- `SIGINT` (Ctrl+C)
- `SIGTERM`

Shutdown flow:
1. Main process broadcasts shutdown signal to runtime loops.
2. Scheduler loop stops creating new jobs.
3. Worker loop stops polling new Kafka messages.
4. Leader loop relinquishes Redis leader lock before exit.
5. In-flight scenario execution is allowed to finish, then worker exits.

Implementation:
- Signal handling: `src/main.rs`
- Loop shutdown checks: `src/application/service.rs`

## Bounded Queues

Pulse uses bounded Kafka client queues to avoid unbounded memory growth:
- Producer queue bound: `queue.buffering.max.messages`
- Consumer prefetch bound: `queued.max.messages.kbytes`

Config:
- `PULSE_QUEUE_CAPACITY` (default `1024`)

Implementation:
- `src/infrastructure/kafka/mod.rs`

## Retry Policy

Each scheduled job now carries:
- `attempt` (starting at `0`)
- `max_retries` (global default from config)

Idempotency keying:
- Duplicate suppression is per `execution_key + attempt`, so retries can execute while still deduplicating redeliveries of the same attempt.

When a scenario run fails:
1. If `attempt < max_retries`, Pulse republishes the job with `attempt + 1`.
2. Backoff is exponential: `base_delay * 2^attempt`, capped at 30s.
3. If retry publish fails, the job is sent to dead-letter queue.

Config:
- `PULSE_WORKER_MAX_RETRIES` (default `2`)
- `PULSE_WORKER_RETRY_BASE_DELAY_MS` (default `500`)

Implementation:
- Retry scheduling: `src/application/service.rs`
- Retry metadata: `src/domain/contracts.rs`

## Dead-Letter Strategy

Pulse publishes failed jobs to a dedicated Kafka DLQ topic when:
- Scenario is unknown on worker.
- Retry publish fails.
- Max retries are exhausted.
- Shutdown occurs before a scheduled retry can be published.

DLQ topic:
- `PULSE_KAFKA_DLQ_TOPIC` (default `pulse.scenario.dlq`)

DLQ record payload:
- `FailedScenarioJob` (`scenario_id`, `run_id`, `execution_key`, `attempt`, `max_retries`, `reason`, timestamp, slice info)

Implementation:
- DLQ publisher: `src/infrastructure/kafka/mod.rs`
- DLQ emission logic: `src/application/service.rs`

## Observability

Additional runtime metrics include:
- Retry publish successes/failures
- DLQ publish successes/failures

Metric names:
- `pulse_worker_retry_jobs_published_total`
- `pulse_worker_retry_job_publish_failures_total`
- `pulse_worker_dlq_published_total`
- `pulse_worker_dlq_publish_failures_total`

Implementation:
- `src/infrastructure/metrics/mod.rs`

## Container and Kubernetes Hardening

Image/runtime:
- Runtime container runs as non-root (`uid/gid 10001`).
- Application files are owned by non-root user.

Kubernetes:
- Pulse pod/container security context:
  - `runAsNonRoot`, fixed user/group, `RuntimeDefault` seccomp
  - dropped Linux capabilities
  - `allowPrivilegeEscalation: false`
  - read-only root filesystem with writable `/tmp` `emptyDir`
- Startup/readiness/liveness probes enabled for:
  - Pulse (`/metrics`)
  - Prometheus (`/-/ready`, `/-/healthy`)
  - Grafana (`/api/health`)
- Resource requests/limits are explicitly set for all core workloads.

Examples:
- HPA example: `k8s/examples/hpa-pulse.yaml`
- Stricter PDB example: `k8s/examples/pdb-pulse.yaml`

Kind note:
- HPA needs metrics-server.
- If unavailable on kind, use `make k8s-fix-metrics-server`.
