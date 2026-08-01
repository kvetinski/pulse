# ADR-0012: Fail-Closed Startup, Health, and Shutdown

- Status: Accepted
- Date: 2026-07-31

## Context

Serving Prometheus metrics proves neither dependency readiness nor worker acceptance.
Malformed environment values previously fell back to defaults, dependency setup could
retry indefinitely, and a process could stay alive after silently skipping required
scenarios. Shutdown also needs an ordering that prevents new dispatch while preserving
Kafka heartbeats and terminal settlement for bounded in-flight work.

## Decision

Runtime configuration is parsed strictly and validated before dependency initialization.
Required scenario/descriptor initialization fails startup unless partial-start mode is
explicitly enabled, and zero initialized scenarios always fails. Dependency setup has a
shutdown-aware startup deadline.

The operations listener exposes three distinct endpoints:

- `/health/live` reports that the process/event loop can answer;
- `/health/ready` succeeds only after configuration, Redis, read-only metadata checks
  for every required Kafka topic and its usable partitions, Kafka producers, Kafka
  consumer, scenario initialization, and worker acceptance are ready, and fails once
  draining begins;
- `/metrics` exposes Prometheus data only.

Shutdown ordering is:

1. mark the process unready and stop renewing/claiming leadership;
2. stop scheduling and accepting new jobs;
3. keep the bounded consumer/worker machinery alive while current work settles;
4. drain until the configured deadline;
5. complete only durably published terminal outcomes;
6. relinquish owned leases when safe and leave ambiguous work uncommitted for recovery;
7. report drain timeout/incomplete work rather than claiming success.

Kubernetes termination grace must exceed the configured application drain deadline.

## Consequences

- Bad configuration and incomplete plans are visible startup failures.
- Liveness is intentionally dependency-independent; dependency outages affect
  readiness and data-plane metrics instead of triggering restart loops by themselves.
- A forced termination after the drain deadline can cause later duplicate target
  execution, but cannot authorize an unacknowledged source offset commit.
- The worker checks shutdown both before waiting for a queued record and immediately
  after receipt, so a record racing with drain start remains uncommitted rather than
  becoming new in-flight work.
