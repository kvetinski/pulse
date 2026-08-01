# ADR-0003: Redis Idempotency Keying by Execution and Attempt

- Status: Superseded by ADR-0007 and ADR-0009
- Date: 2026-03-06

## Context

Kafka consumers may receive duplicate deliveries due to retries/rebalances. Pulse must avoid duplicate scenario execution while still allowing explicit retry attempts.

## Decision

This original decision used a claimed-before-execution TTL key. It is retained as
historical context only. Current execution records are renewable owner-checked leases
with running/terminal state, expiry recovery, and terminal retention; a successful
claim is not evidence of completion.

## Historical Consequences

- Duplicate deliveries for the same attempt are suppressed.
- Retries are still possible because each attempt has a distinct idempotency key.
- Protection is bounded by TTL; extremely late duplicates after expiry can re-execute.

## Considered Alternatives

1. Idempotency keyed only by `execution_key`.
- Pros: simpler key model.
- Cons: blocks legitimate retries because all attempts collapse to one key.

2. No idempotency (at-least-once only).
- Pros: no Redis claim path.
- Cons: duplicate scenario execution and metric distortion under consumer churn.

3. Exactly-once transactional processing end-to-end.
- Pros: strongest semantics.
- Cons: high complexity and cross-system transactional coupling outside current scope.
