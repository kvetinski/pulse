# Distributed Pulse Rollout Plan

## Phase 1: Baseline deployment
- Deploy pulse with Redis + Kafka configured.
- Run with 1 replica and verify jobs/results topics.

## Phase 2: Enable HA scheduling
- Scale pulse to 3 replicas.
- Verify one active leader and worker group consumption.

## Phase 3: Controlled scenario migration
- Migrate low-rate scenarios first.
- Observe logs and result topic for failures/duplicates.

## Phase 4: Scale and tune
- Increase partitions for jobs topic.
- Increase pulse replicas.
- Tune leader TTL, renew interval, scheduler tick.

## Phase 5: Production hardening
- Add alerting for leader churn and consumer lag.
- Add DLQ handling policy and replay tooling.
