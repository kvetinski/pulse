# Changelog

All notable changes to this project will be documented in this file.

The format is based on Keep a Changelog and this project follows Semantic Versioning for release tags.

## [Unreleased]

### Added

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

## [0.1.0] - 2026-03-06

### Added

- Initial public versioned release line for Pulse.
- Distributed scheduler/worker runtime with:
  - Redis leader election + idempotency
  - Kafka job/results/DLQ topics
  - dynamic gRPC scenario execution from descriptor sets
  - per-step and scenario metrics, Docker and Kubernetes deployment support
