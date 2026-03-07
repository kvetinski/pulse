# ADR-0004: Kafka Partition Key Strategy is Scenario-Configurable

- Status: Accepted
- Date: 2026-03-06

## Context

Different workloads need different tradeoffs between partition spread and ordering locality.

## Decision

Expose partition strategy per scenario:

- `execution_key` (default) for broader partition distribution.
- `scenario_id` for stronger per-scenario locality.

## Consequences

- Runtime behavior can be tuned without code changes.
- Users can choose throughput-oriented or locality-oriented behavior per scenario.
- Misconfigured strategy can reduce parallelism or locality, depending on workload goals.

## Considered Alternatives

1. Always use `execution_key`.
- Pros: generally better spread.
- Cons: cannot enforce stronger scenario-level locality when needed.

2. Always use `scenario_id`.
- Pros: stable partition affinity per scenario.
- Cons: can underutilize partitions for high-volume scenarios.

3. Automatic strategy selection.
- Pros: potentially adaptive behavior.
- Cons: opaque behavior and harder operator predictability/debugging.
