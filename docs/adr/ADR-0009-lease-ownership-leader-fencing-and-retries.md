# ADR-0009: Lease Ownership, Leader Fencing, and Retry Classification

- Status: Accepted
- Date: 2026-07-31

## Context

A Redis TTL alone does not establish ownership. A paused worker can resume after its
lease expires, and a paused scheduler can resume after a new leader is elected. Both
are stale actors even if they still hold an in-memory value that once represented a
valid lease. Separately, retrying every gRPC error turns target measurements into
extra traffic and changes the requested load.

## Decision

Leadership and execution claims return typed outcomes. Redis time is authoritative,
dependency failures remain errors, and every acquired lease contains an opaque owner
token. Leader leases also carry a monotonically increasing fencing token. Renewal,
dispatch-ledger mutation, completion, and release are atomic owner-checked Lua
operations.

An expired execution lease may be recovered by a redelivery. The new worker receives
a new owner token; a stale worker cannot renew, release, or complete it. Terminal
execution records remain for a configured retention TTL so redelivery can distinguish
a verified completed outcome from active work.

Local lease budgets are monotonic and begin before each Redis claim or renewal request;
network/queueing latency therefore reduces the remaining budget. Claims and renewals
whose responses arrive after the conservative deadline fail closed, renewal cadence is
anchored to request starts, and an in-flight renewal is raced against the previously
established local deadline so target execution is cancelled before uncertain ownership
can continue. Every terminal pre-send owner check applies the same rule. Configuration
also requires each TTL to cover both missed-renewal tolerance and the maximum Redis
operation-response budget.

Retries use this classification:

- target status, transport failure, and request deadline are measurements in the
  result and do not retry the load slice;
- malformed/unsupported contracts and unknown initialized scenarios are permanent
  processing failures and require an acknowledged DLQ record;
- Redis claim errors, renewal errors, stale ownership, and ambiguous publication
  settlement leave the input unsettled; no retry is inferred from an uncertain lease;
- every leased result, retry, and DLQ send attempt is preceded by an owner-checked
  renewal, so an already-observed stale owner cannot publish follow-on work;
- terminal-event publication is retried a bounded number of times with exponential
  backoff and deterministic jitter while a separate bounded consumer pump keeps Kafka
  polling;
- exhausting the result-publication budget leaves the source unsettled and does not
  publish a whole-slice retry, because the target traffic has already run;
- after a classified Pulse infrastructure execution failure, Pulse publishes
  deterministic attempt N+1 to the normal Kafka jobs topic;
- the retry record carries a persisted `not_before_unix_ms`, incremented attempt, the
  same execution identity, deterministic plan fingerprint, and unchanged slice plan;
- Kafka acknowledgement precedes owner-checked `retry_published` completion and source
  offset commit. Exhausting the job retry ceiling requires an acknowledged DLQ record.

Pulse does not use a separate delayed topic or broker-side timer. A worker consuming a
future `not_before_unix_ms` waits with a bounded, shutdown-aware local timer before
claiming the execution lease. This makes retry intent crash-durable, but can
head-of-line block one processor for at most the configured retry delay. The bounded
consumer pump remains independent and can continue polling until its handoff is full.

## Consequences

- A Redis outage creates backpressure; it never looks like a duplicate or successful
  claim.
- The final Redis owner check and Kafka acknowledgement are not atomic. A pause in that
  interval can still produce a deterministic stale output; Redis completion prevents
  the stale source commit, result aggregation ignores duplicate event IDs, and an
  orphan retry can add traffic. A fenced execution outbox is required to eliminate
  this remaining interval.
- Ambiguous failures prefer duplicate work over silent loss.
- Target-side effects can still be duplicated across the target/Kafka crash boundary.
- Fencing protects Pulse coordination state, not an external target that does not
  honor a Pulse fencing or idempotency token.
- Both deferred retry jobs and unsettled failures can delay later partition work.
  Operators alert on `pulse_worker_retry_job_age_seconds`, general job age, and Kafka
  lag; a future broker-native delay design would require a separate ADR.
