# ADR-0007: Failure Model and Delivery Semantics

- Status: Accepted
- Date: 2026-07-31
- Supersedes: ADR-0003 and ADR-0006

## Context

Pulse crosses three independent consistency domains: Kafka, Redis, and the target gRPC
service. There is no transaction that can atomically combine a target side effect, a
Kafka output record, a Redis coordination transition, and a Kafka input-offset commit.
Treating a Redis error as a duplicate, or committing after an output publication
failure, loses work. Treating a target status as an engine failure amplifies load by
replaying an entire slice.

## Decision

Pulse provides:

> At-least-once target execution with deterministic job identities, lease-based
> duplicate suppression, durable terminal-event publication, and duplicate-tolerant
> result aggregation.

Pulse does not claim exactly-once target execution. A crash after a target side effect
and before the durable terminal record can execute that side effect again. Scenarios
with externally visible side effects must therefore use target-side idempotency keys or
be explicitly accepted as replay-safe by the operator.

The following invariants are normative for code, tests, runbooks, and alerts:

1. A dependency error is never interpreted as `duplicate`, `not due`, or `success`.
2. A consumed Kafka offset is not committed until the job reaches a durably
   acknowledged terminal disposition.
3. Terminal dispositions are a result, retry, or DLQ record acknowledged by Kafka, or
   a verified duplicate whose durable terminal outcome already exists.
4. A result, retry, or DLQ publication failure leaves the source offset uncommitted.
5. A schedule window is complete only after every deterministic slice publication is
   acknowledged in the dispatch ledger.
6. A scheduler stops initiating publications when it observes lease loss or reaches
   its conservative local lease deadline; every dispatch-ledger mutation is
   owner/fence checked. A Kafka send initiated while the lease was valid can still
   acknowledge after expiry and is handled as an ambiguous deterministic duplicate.
7. Target-service statuses and request deadlines are measurements, not reasons to
   retry a whole load slice.
8. Automatic job retries are reserved for classified Pulse infrastructure failures.
9. Queues, delayed retries, task sets, and shutdown draining are bounded.
10. Cross-system exactly-once target execution is explicitly out of scope.

## Job lifecycle

```text
Kafka job
   |
   v
decode/validate -- poison or permanent invalid --> Kafka DLQ ack --> sync commit
   |
   v
acquire Redis execution lease
   |-- dependency error ------> fail-stop; keep offset uncommitted
   |-- busy ------------------> retain in place; re-check within Kafka-safe bound
   |-- durable terminal ------> verified duplicate ----------------------> sync commit
   v
execute target calls while renewing lease
   |-- target status/deadline --> include as failed measurement
   |-- classified Pulse infra --> Kafka retry ack --> Redis terminal ----> sync commit
   v
Kafka result ack --> Redis terminal completion --> synchronous offset commit
```

Publication precedes the Redis terminal transition. This ordering means an
acknowledged Kafka output can be duplicated if the process crashes before recording
completion or committing the source offset. It cannot be silently lost. Output keys
are deterministic and aggregation ignores duplicate execution identities.

For leased result, retry, and DLQ paths, Pulse performs an owner-checked Redis renewal
immediately before every Kafka send attempt. A stale owner or Redis error therefore
fails closed before the send. Redis and Kafka still cannot make that check and the
subsequent broker acknowledgement atomic: a process paused in that narrow interval can
publish a deterministic stale output. Redis completion then rejects the stale owner,
so the source cannot commit. Result consumers deduplicate the event; a rare orphan
retry can add target traffic. Eliminating this interval requires a fenced durable
execution outbox or Kafka transaction design and is not claimed by this model.

## Offset settlement

Workers return an explicit disposition. Only `ResultPublished`, `RetryPublished`,
`DeadLetterPublished`, and `DuplicateCompleted` are committable. `RetryLater`,
`ExecutionLeaseBusy`, cancellation, lease loss, and every coordination/publication
error are not. `ExecutionLeaseBusy` is handled separately from dependency failure: the
worker keeps that record at the head of its processor, leaves its offset uncommitted,
and rechecks Redis in bounded, shutdown-aware intervals. It may commit only after it
observes a retained terminal outcome (`DuplicateCompleted`) or acquires an expired
lease and executes the job. The outer Kafka-safe processing deadline remains the hard
bound; expiry of that budget fails closed without processing a later queued record.

Pulse does not move a busy record behind later work. Kafka commits are cumulative per
partition, so doing so could allow a later commit to settle past the unresolved offset.
The separate bounded intake pump continues polling while the processor waits.

Kafka commits use broker-acknowledged synchronous commits. This costs a round trip per
settled record, but gives shutdown and rebalance logic evidence that the broker accepted
the offset. Batching or transactions may be introduced only with equivalent
partition-ordering tests.

Each fetched job/result also captures the consumer assignment epoch. A rebalance
increments that epoch before revocation/assignment callbacks; commit checks both the
epoch and current topic/partition assignment before and after the synchronous broker
request. Buffered work from an old epoch therefore cannot be intentionally committed
through the consumer's new generation. Its durable deterministic output may already
exist, so redelivery settles through the lease/result dedupe paths.

## Failure semantics

| Failure point | Recovery behavior | Offset committed? | Duplicate possible? |
| ------------- | ----------------- | ----------------: | ------------------: |
| Redis unavailable during claim | Fail-stop; Kafka redelivers after restart/rebalance | No | No target call yet |
| Execution lease is held by another worker | Retain record in place; recheck for terminal completion or lease expiry | Only after verified completion/recovery | No re-execution while lease is live |
| Crash after lease acquisition | Lease expires; redelivery acquires a recovery lease | No | Yes |
| Target returns non-OK status | Publish one failed measurement for the slice | After durable result | No automatic slice retry |
| Request deadline expires | Publish a timeout measurement | After durable result | No automatic slice retry |
| Result publication exhausts local retries | Fail-stop with the source unsettled; do not replace the failed result disposition with a whole-slice retry | No | Yes on source recovery |
| Retry publication fails | Keep source unsettled; DLQ only if its publication succeeds | No, unless DLQ succeeds | Yes |
| DLQ publication fails | Keep source unsettled | No | No completed disposition |
| Output acknowledged, completion write fails | Redelivery may publish the same deterministic output | No | Yes; aggregator ignores it |
| Lease expires after final owner check, before Kafka ack | Completion rejects stale owner; source redelivers | No | Yes; deterministic output, and an orphan retry can add traffic |
| Completion written, offset commit fails | Redelivery verifies terminal completion | No proof of commit | No target re-execution |
| Poison Kafka payload | Publish deterministic raw-payload DLQ envelope | After DLQ ack | DLQ duplicate possible |
| Shutdown during execution | Stop intake, renew/drain to deadline, otherwise leave unsettled | Only terminal work | Yes |

## Error classification

- **Target measurement:** gRPC status, target connection failure, and request deadline.
  These affect scenario success metrics and result records.
- **Permanent processing failure:** unsupported contract, malformed slice metadata,
  unknown scenario, or invalid initialized plan. These go to the DLQ.
- **Transient Pulse infrastructure failure:** Redis/Kafka timeout or unavailability.
  These leave the source unsettled or publish a bounded durable retry.
- **Cancellation/lease loss:** no success is inferred and no source commit is made.
- **Invariant violation:** fail closed, emit an operational error, and do not commit.

Retries can add target traffic when an infrastructure failure occurs after target
execution but before its terminal event is durable, or in the narrow cross-system
lease-check/Kafka-ack ambiguity above. This is an unavoidable consequence of the
selected at-least-once model and must be included in evidence and capacity planning.

## Consequences

- Work is biased toward duplication instead of loss at ambiguous crash boundaries.
- Redis availability is on the execution path and outages create backpressure rather
  than false duplicate success.
- Synchronous commits and terminal Redis writes add latency.
- Target APIs with non-idempotent side effects require their own idempotency mechanism.
- Result consumers must remain duplicate tolerant for at least the terminal-retention
  period.
