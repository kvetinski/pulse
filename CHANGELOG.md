# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning for release tags.

## [Unreleased]

## [0.2.0] - 2026-08-01

### Added

- Failure-model ADRs defining the at-least-once delivery guarantee, crash boundaries,
  execution leases, recoverable dispatch, aggregation, bounded work, and shutdown.
- Atomic owner-checked Redis execution leases and leader-fenced dispatch ledgers.
- Versioned result contracts with duplicate-tolerant mergeable latency aggregation.
- Strict runtime configuration, gRPC deadlines, safety ceilings, dry-run planning,
  target allowlisting, and separate liveness/readiness endpoints.
- Deterministic paused-time and coordination failure tests.
- A self-contained two-replica reviewer demo with a deterministic gRPC fixture,
  timestamped topology/event replay, and machine-readable settlement evidence.
- A self-contained kind overlay with the same repository-owned gRPC fixture, recurring
  healthy/expected-failure traffic, per-replica Prometheus discovery, and matching
  soak/performance evidence inputs.
- Reliability/performance workflow:
  - soak/chaos runner command
  - runtime perf threshold gate
  - structured perf JSON output and local history JSONL
  - visual perf markdown report and trend charts
  - cumulative history HTML page and CI artifact publishing
- Grafana dashboard perf-gate trend panels with threshold lines.
- Grafana annotation support for perf-gate runs (commit/tag markers).
- Kubernetes and Docker Compose persistence for observability data.
- ADR structure in `docs/adr/` with decision records and alternatives.
- Pod security baseline document.
- Supply-chain security checks in CI:
  - dependency vulnerability scan (`cargo audit`)
  - container image scan (Trivy)
  - SBOM generation (SPDX JSON)

### Changed

- Kafka source offsets are synchronously committed only after an acknowledged terminal
  result/DLQ disposition or a verified durable duplicate.
- Target-service failures are recorded as measurements rather than automatically
  replaying a whole load slice.
- Fractional rates and cross-slice concurrency now preserve the configured global load.
- Kubernetes base manifests are application-only; demo infrastructure is isolated from
  staging and production-oriented examples.
- Patched `anyhow` and `rand` dependency releases selected by the lockfile security
  refresh.

### Fixed

- Redis failures can no longer be interpreted as duplicate work or a successful due
  check.
- Partial schedule publication is resumed using deterministic window and slice
  identities.
- Result and DLQ publication failures leave the source Kafka record unsettled.
- Completed runner tasks are continuously reaped so long runs remain bounded.
- Consumed Kafka records are byte-bounded before owned decode/queue copies, poison
  evidence retains bounded prefixes, and assignment epochs fence commits across
  rebalances.
- Leased result/retry/DLQ publication rechecks ownership before every send attempt;
  renewal uncertainty no longer creates follow-on work.
- Lease validity and renewal cadence are anchored before Redis requests, so slow
  successful responses cannot extend stale local ownership.
- Pending execution renewals are raced against the current monotonic deadline, so a
  slow Redis response cancels target work before ownership becomes uncertain.
- Scenario task panics become fail-stop invariant violations, and result aggregation
  is bounded by the validated Kafka-safe processing interval. Invariant violations
  dominate concurrent infrastructure failures and can never trigger load retries.
- Dispatch fingerprints now bind engine version, gRPC deadlines, and descriptor-set
  contents in addition to job payload and retry semantics.
- Compose commands select and reuse an unused private subnet, including on Docker
  daemons whose default address pools are exhausted.

## [0.1.2] - 2026-03-10

### Changed

- Removed the vulnerable protobuf dependency path and made local supply-chain checks
  safer and more reproducible.

## [0.1.1] - 2026-03-07

### Fixed

- Corrected semantic version validation in the release-tag command.

## [0.1.0] - 2026-03-07

### Added

- Initial public versioned release line for Pulse.
- Distributed scheduler/worker runtime with:
  - Redis leader election + idempotency
  - Kafka job/results/DLQ topics
  - dynamic gRPC scenario execution from descriptor sets
  - per-step and scenario metrics, Docker and Kubernetes deployment support
