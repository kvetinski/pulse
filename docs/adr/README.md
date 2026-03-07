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
