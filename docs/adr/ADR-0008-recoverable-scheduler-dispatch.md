# ADR-0008: Recoverable Scheduler Dispatch Ledger

- Status: Accepted
- Date: 2026-07-31

## Context

Marking a scenario due before publishing all Kafka slices loses a `repeat: once` run or
part of a repeated window when a publish fails. Leader failover also needs a stable
window identity and evidence of which slices Kafka acknowledged.

## Decision

Use a Redis dispatch ledger with one active window per scenario. The ledger stores the
scheduled window, contract version, plan fingerprint, deterministic run identity, total
slice count, and acknowledged slice indexes. The fingerprint includes every setting
that changes a job payload, including rate, duration, concurrency, startup burst,
partition strategy, steps, and the worker retry ceiling. It also binds execution
semantics that can change the meaning of the same payload: Pulse package version,
request/scenario deadlines, and a deterministic fingerprint of the descriptor-set
contents. Mixed plans therefore fail closed rather than executing one identity under
different schemas or deadlines.

The scheduler:

1. prepares or resumes the active due window under the current leader token/fence;
2. derives each execution identity from scenario ID, scheduled window, slice index,
   total slices, and contract version;
3. publishes only slices not acknowledged in the ledger;
4. records a slice acknowledgement only after Kafka acknowledges publication; and
5. marks the window complete and advances `next_at`/`once_done` only after all slice
   acknowledgements exist.

Every ledger mutation verifies current Redis leader ownership. The scheduler converts
the Redis-returned TTL into a conservative Tokio monotonic deadline anchored before
the acquire/renew request, so response latency consumes rather than extends local
validity. Renewal cadence is likewise anchored to request starts. The scheduler checks
its local leadership watch before each slice and cancels in-flight delivery waiting
when that deadline or observed ownership is lost. This avoids comparing Redis time
with a different host's wall clock. Kafka publication and the Redis
acknowledgement cannot be atomic: if Kafka accepts a slice and the acknowledgement is
lost, the next leader republishes the same deterministic slice. Execution leases and
duplicate-tolerant aggregation make that safe.

A send initiated while leadership was valid may be acknowledged by Kafka after the
lease expires; cancelling a delivery future cannot retract a broker-accepted record.
The expired leader cannot acknowledge that slice in Redis. Recovery therefore treats
the physical record as an ambiguous deterministic duplicate rather than pretending
cross-system fencing is atomic.

## Alternatives considered

### Kafka-first scheduling

Publishing a complete manifest to Kafka would make Kafka the scheduling source of
truth. It reduces Redis state but still needs a mechanism to expand and verify all
slices, and complicates the current single-binary scheduler/worker design.

### Transactional outbox

Writing schedule state and an outbox row in one transactional database gives a clean
publication boundary. Pulse does not otherwise require such a database, and adding one
only for dispatch would expand the operational surface before the current runtime is
correct.

### Redis dispatch ledger (chosen)

This reuses the existing coordination dependency and is the smallest design satisfying
recoverable partial publication. It deliberately offers at-least-once dispatch rather
than attempting cross-system atomicity.

## Rate and concurrency distribution

Slice rates are an exact equal division of the configured global scenario rate.
Concurrency uses quotient and remainder: slice `i` receives `base + 1` when
`i < remainder`, otherwise `base`. The sum never exceeds or falls below the configured
global concurrency, and no per-slice rate floor is applied.

## Consequences

- A once-only run cannot be declared done before every slice is acknowledged.
- Failover resumes missing slices with identical identities.
- An ambiguous Kafka-ack/Redis-ack boundary can duplicate a slice but cannot lose it.
- Active-window configuration drift is rejected through the persisted plan fingerprint
  instead of silently changing the remaining load.
- Operators resolve active-window drift by restoring the matching configuration until
  the missing slices settle. Readiness currently reports runtime dependency/loop
  health, not per-scenario dispatch-ledger compatibility; the runbook describes the
  explicit recovery path.
