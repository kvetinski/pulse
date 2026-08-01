# Operational Safety

Pulse is an experimental, production-oriented distributed-systems project. This
document describes implemented safeguards and the remaining operator obligations; it
is not a production-readiness claim.

## Delivery Guarantee

> At-least-once target execution with deterministic job identities, lease-based
> duplicate suppression, durable terminal-event publication, and duplicate-tolerant
> result aggregation.

Pulse cannot atomically commit an external gRPC side effect with Kafka and Redis. A
crash after a target side effect but before durable result publication can execute the
side effect again. Side-effecting targets need their own idempotency key or an explicit
operator decision that replay is safe. See
[`ADR-0007`](adr/ADR-0007-failure-model-and-delivery-semantics.md).

## Offset and Publication Safety

A source Kafka offset is synchronously committed only after one of these terminal
dispositions:

- Kafka acknowledged a result record and Redis owner-checked completion succeeded;
- Kafka acknowledged a deterministic attempt N+1 retry record and completion recorded
  `retry_published` for attempt N;
- Kafka acknowledged a structured or poison-message DLQ record; or
- Redis proves that the deterministic execution already has a durable terminal state.

Redis unavailability, a busy lease, lease loss, result/DLQ publication failure,
cancellation, and commit failure are not terminal success. They leave the source record
unsettled. Kafka output keys and event IDs are deterministic because a crash after
output acknowledgement but before Redis completion or offset commit can republish the
same output.

The dedicated result consumer follows the same fail-closed rule: it synchronously
commits a result offset only after Redis durably accepts or verifies the slice. Complete
and timed-out run summaries are first written to a Redis outbox, then published to the
summary topic. A crash between Kafka acknowledgement and outbox acknowledgement may
republish the same deterministic summary revision; consumers must deduplicate by
`event_id` and revision.

## Atomic Execution Leases

Execution claims are one atomic Redis operation and contain an opaque owner token,
attempt, state, and expiry. Long jobs renew their lease. Renewal, completion, and
release verify ownership atomically, so a stale worker cannot modify a lease recovered
by another worker. Expired work can be reclaimed, while completed outcomes remain for
`PULSE_EXECUTION_TERMINAL_RETENTION_MS`.

Leader leases use a separate opaque token and monotonic fence. Dispatch-ledger writes
verify both, and the scheduler checks leadership before each slice. Losing leadership
stops new dispatch; an ambiguous Kafka-ack/Redis-ledger-ack boundary may duplicate a
deterministic slice but cannot mark an unpublished slice complete.

## Bounded Work and Retry Behavior

The following controls are intentionally separate:

- `PULSE_KAFKA_PRODUCER_QUEUE_MESSAGES`: producer message count;
- `PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES`: serialized producer record limit, sized to
  carry bounded base64 poison evidence;
- `PULSE_KAFKA_CONSUMER_QUEUE_KBYTES`: librdkafka prefetch memory in KiB;
- `PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES`: initial per-partition fetch size;
- `PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES`: total fetch response size;
- `PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES`: hard combined key/payload ownership and
  decode limit for one consumed record;
- `PULSE_RETRY_QUEUE_CAPACITY`: in-process consumer-to-worker handoff bound;
- `PULSE_AGGREGATION_MAX_ACTIVE_RUNS`, `PULSE_AGGREGATION_SCAN_BATCH`, and
  `PULSE_AGGREGATION_MAX_ERROR_KINDS`: durable aggregation bounds;
- `PULSE_MAX_CONCURRENCY`: operator safety ceiling for a scenario;
- `PULSE_WORKER_MAX_RETRIES`: automatic infrastructure retry ceiling and bounded local
  terminal-publication attempt budget;
- `PULSE_WORKER_RETRY_BASE_DELAY_MS` and
  `PULSE_WORKER_RETRY_MAX_DELAY_MS`: exponential backoff bounds;
- `PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS`: finite shutdown drain.

Kafka partition fetch byte settings are soft for the first oversized batch. Pulse
hard-bounds its response allocation and rejects an oversized consumed record before
retaining an owned full key/payload copy; enforce a compatible hard broker/topic
message-size limit too. Pulse bounds DLQ amplification by retaining only
deterministic 256 KiB key/payload prefixes with original lengths and truncation flags;
the omitted suffix is unavailable from the DLQ record.

The bounded consumer pump continues polling while the worker settles one source record,
so a publication backoff does not directly stop Kafka polling. The processor retains
partition order: it will not commit later records after an earlier synchronous commit
failure. Retry intent is durably published to the normal Kafka jobs topic with a
`not_before_unix_ms`; there is no separate broker-delayed topic. The shutdown-aware
local deferral can head-of-line block one processor, while the independent bounded pump
continues polling until its handoff is full. After retry/DLQ publication or completion
exhausts its bounded local budget, the worker fail-stops with the source uncommitted.
Monitor consumer lag, job age, and `pulse_worker_retry_job_age_seconds` accordingly.

Aggregation deadline and outbox scans are bounded. Incomplete runs become `timed_out`
after `PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS`; a late missing slice can produce a newer
`complete` revision while retained state exists. A stale outbox acknowledgement cannot
erase that revision. Redis `TIME` is authoritative for durable aggregation timestamps,
deadline scans, and expiry, so application-host clock skew does not move the timeout.

Target gRPC status, transport errors, and deadlines are recorded as load measurements
in the result. They do not automatically replay an entire slice. Automatic recovery is
reserved for Pulse infrastructure/settlement failures, which can add traffic only at
an unavoidable ambiguous crash boundary.

## Rate and Target Safety

- Fractional scenario-start rates are supported; the default pacing has no multi-token
  startup burst.
- `PULSE_STARTUP_BURST` is an explicit opt-in burst and cannot exceed the global
  concurrency ceiling.
- Slice rate and concurrency allocations sum to their configured global values.
- `PULSE_DRY_RUN=true` prints the deterministic plan without target traffic.
- Non-local target hosts must be listed in `PULSE_TARGET_ALLOWLIST` or explicitly
  accepted with `PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS=true`.
- Duration, rate, concurrency, queue sizes, retry counts, and timeout relationships are
  fail-fast validated.

## Health Endpoints

- `GET /health/live`: the process/event loop is answering; dependency outages do not
  deliberately trigger a restart loop.
- `GET /health/ready`: configuration, Redis, required Kafka topic/partition metadata,
  Kafka producers and consumer, initialized scenarios, and worker acceptance are ready;
  returns non-success during startup or shutdown drain.
- `GET /metrics`: Prometheus exposition only.

Kubernetes probes use the health endpoints rather than `/metrics`.

## Graceful Shutdown

Pulse handles `SIGINT` and `SIGTERM` in this order:

1. become unready and stop claiming/renewing scheduler leadership;
2. stop scheduling new windows;
3. stop accepting new jobs while bounded polling/heartbeat machinery winds down;
4. drain current work up to `PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS`;
5. publish and commit only terminal dispositions that can still be acknowledged;
6. release owned leases when safe and leave ambiguous work uncommitted; and
7. log incomplete work at the deadline.

Kubernetes `terminationGracePeriodSeconds` is longer than the configured work-drain
budget plus the derived scheduler/coordination/broker cleanup grace.
A forced kill can cause later duplicate target execution; it must not turn unsettled
work into a committed success.

## DLQ Retention and Replay

Retention and the operator-confirmed replay workflow are defined in
[`docs/dlq-operations.md`](dlq-operations.md). Replay is allowed only after the root
cause is fixed and the target operation is confirmed idempotent/replay-safe. The replay
tool defaults to dry-run and requires explicit confirmation before publishing.

## Deployment Boundaries

`k8s/base` contains only Pulse application resources. `k8s/demo` contains the
repository-owned deterministic gRPC target, single-node Kafka/Redis,
Prometheus/Grafana, and local storage and is included only by the kind overlay. These
demo services are intentionally not a highly available or durable production
architecture.

Staging and production-oriented overlays expect managed Kafka, managed Redis, and an
external monitoring stack. Topic creation is opt-in and disabled there. Images use an
explicit version placeholder and should be replaced by a reviewed immutable digest.
Redis credentials are expected through the mounted secret file.

Important current security limitation: the Rust configuration does not yet expose
Kafka SASL/TLS or Redis TLS certificate/authentication options beyond the Redis URL.
Dynamic gRPC transport is plaintext HTTP/2 only: this build rejects `https://` because
tonic TLS transport, custom CA, and mTLS are not enabled. Therefore the staging/prod
manifests are topology and hardening examples, not directly deployable production
configurations. Do not route credentials or target traffic over untrusted networks.

The Pulse container remains non-root with dropped capabilities, a read-only root
filesystem, bounded resources, and a writable `/tmp` `emptyDir`.
