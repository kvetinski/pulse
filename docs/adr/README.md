# Architecture Decision Records

This directory contains Architecture Decision Records (ADRs) for Pulse.

## Conventions

- Status: `Accepted`, `Proposed`, `Superseded`.
- File naming: `ADR-<number>-<slug>.md`.
- ADRs are append-only. If a decision changes, create a new ADR and reference the superseded one.

## ADR Index

- [ADR-0001 Scheduler/Worker Split](ADR-0001-scheduler-worker-split.md)
- [ADR-0002 Redis Leader Election](ADR-0002-redis-leader-election.md)
- [ADR-0003 Redis Idempotency Keying](ADR-0003-redis-idempotency-execution-attempt.md)
- [ADR-0004 Kafka Partition Key Strategy](ADR-0004-kafka-partition-key-strategy.md)
- [ADR-0005 Multi-Layer Backpressure](ADR-0005-multi-layer-backpressure.md)
- [ADR-0006 Worker Retry and DLQ Strategy](ADR-0006-worker-retry-and-dlq.md)
- [ADR-0007 Failure Model and Delivery Semantics](ADR-0007-failure-model-and-delivery-semantics.md)
- [ADR-0008 Recoverable Scheduler Dispatch](ADR-0008-recoverable-scheduler-dispatch.md)
- [ADR-0009 Lease Ownership, Leader Fencing, and Retry Classification](ADR-0009-lease-ownership-leader-fencing-and-retries.md)
- [ADR-0010 Duplicate-Tolerant Run Aggregation](ADR-0010-duplicate-tolerant-run-aggregation.md)
- [ADR-0011 Rate, Concurrency, and Bounded Work](ADR-0011-rate-concurrency-and-bounded-work.md)
- [ADR-0012 Fail-Closed Startup, Health, and Shutdown](ADR-0012-startup-health-and-shutdown.md)
