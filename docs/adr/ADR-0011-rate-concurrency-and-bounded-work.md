# ADR-0011: Rate, Concurrency, and Bounded Work

- Status: Accepted
- Date: 2026-07-31

## Context

Integer token refill imposed a one-scenario-per-second floor, initial bucket capacity
created an undeclared burst, and ceiling division could assign more concurrency across
slices than the scenario requested. Retaining every completed task in a `JoinSet`
also allowed scheduler metadata to grow throughout a long launch window.

## Decision

Pulse uses monotonic Tokio time and fractional token accumulation. Smooth pacing is the
default; `PULSE_STARTUP_BURST` is the only way to request initial burst capacity.
Paused-time tests cover sub-one rates such as `0.1` scenarios/second.

The scheduler divides the global rate equally across deterministic slices. It divides
concurrency by quotient and remainder, so the slice allocations sum exactly to the
configured global limit. Zero-concurrency slices are not produced.

The runner uses its `JoinSet` as the concurrency gate and continuously reaps completions
while it waits for pacing or capacity. Kafka prefetch, the handoff channel, publication
attempts, task sets, active aggregations, and shutdown drain time all have explicit
bounds. Configuration rejects non-finite rates, zero capacities, and loads above the
operator safety ceilings. Dry-run planning computes the same slice allocations without
sending traffic.

## Consequences

- Low-rate scenarios preserve their intended inter-arrival interval.
- The default does not front-load a full concurrency window at startup.
- Total cross-slice concurrency never exceeds the declared scenario concurrency.
- A slow partition can apply bounded backpressure rather than consume unbounded memory.
- The limiter governs scenario starts, not individual RPC calls inside a scenario.
