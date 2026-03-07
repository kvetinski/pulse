# ADR-0003: Redis Idempotency Keying by Execution and Attempt

- Status: Accepted
- Date: 2026-03-06

## Context

Kafka consumers may receive duplicate deliveries due to retries/rebalances. Pulse must avoid duplicate scenario execution while still allowing explicit retry attempts.

## Decision

Use Redis idempotency keys keyed by `execution_key:attempt-<n>`, with TTL, claimed before job execution.

## Consequences

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
