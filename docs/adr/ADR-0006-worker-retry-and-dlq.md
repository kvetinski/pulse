# ADR-0006: Worker-Level Retry and DLQ Strategy

- Status: Accepted
- Date: 2026-03-06

## Context

Scenario execution can fail due to transient target errors or infrastructure issues. Pulse needs bounded retries and a clear sink for exhausted failures.

## Decision

Use worker-level, job-based retries with exponential backoff and publish exhausted or retry-publish-failed jobs to a dedicated DLQ topic.

## Consequences

- Improves resilience to transient failures.
- Makes unrecoverable failures observable and replayable through DLQ operations.
- Retries apply per scenario job, not per individual step (current limitation).

## Considered Alternatives

1. No retries, immediate DLQ.
- Pros: simplest behavior and low retry pressure.
- Cons: poor resilience to transient failures.

2. Unlimited retries.
- Pros: may eventually recover without DLQ.
- Cons: unbounded load amplification and delayed failure visibility.

3. Per-step retry policies only.
- Pros: finer control at step level.
- Cons: larger configuration and execution complexity; deferred to future work.
