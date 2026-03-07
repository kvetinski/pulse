# ADR-0005: Multi-Layer Backpressure

- Status: Accepted
- Date: 2026-03-06

## Context

Pulse must avoid runaway load against target systems while keeping configured traffic shape.

## Decision

Apply backpressure at multiple layers:

- token bucket for scenario start rate (`scenarios_per_sec`),
- semaphore for in-flight cap (`max_concurrency`),
- bounded producer/consumer queues and pull-based worker processing.

## Consequences

- Prevents unlimited in-flight growth when downstream slows.
- Effective throughput degrades gracefully under saturation.
- Operators must tune rate/concurrency with expected scenario latency.

## Considered Alternatives

1. Rate limiting only (no concurrency cap).
- Pros: simpler configuration.
- Cons: can still create excessive in-flight work under high latency.

2. Concurrency cap only (no rate shaping).
- Pros: simpler implementation.
- Cons: bursty starts and less stable workload shape.

3. Queue-length-only throttling.
- Pros: directly tied to backlog.
- Cons: delayed feedback and less explicit operator intent than scenario rate/concurrency controls.
