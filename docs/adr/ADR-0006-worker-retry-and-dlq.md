# ADR-0006: Worker-Level Retry and DLQ Strategy

- Status: Superseded by ADR-0007 and ADR-0009
- Date: 2026-03-06

## Context

This record captured the prototype policy and is retained as historical context.
Target failures and Pulse infrastructure failures are now separate classes.

## Decision

The current runtime records target failures as results without whole-slice retry. It
retries terminal publication locally with bounded exponential backoff and jitter; if
settlement cannot become durable, it retains the uncommitted source. Permanent and
poison records require an acknowledged DLQ publication. There is no current delayed
retry-topic adapter.

## Historical Consequences

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
