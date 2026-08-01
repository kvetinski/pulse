# Compatibility Policy

This document defines release compatibility expectations for Pulse.

## Versioning and Tags

- Release tags use semantic format: `vMAJOR.MINOR.PATCH` (example: `v0.1.2`).
- Source of truth for released changes is `CHANGELOG.md`.
- `Cargo.toml`, the Pulse entry in `Cargo.lock`, `CHANGELOG.md`, the Docker Rust
  builder, CI, and `rust-toolchain.toml` are checked by `make release-validate`.
- After updating `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, and the example image tags
  to the next version, validate and create the tag:

```bash
make release-validate VERSION=0.2.0
make release-tag VERSION=0.2.0
```

- Push tag:

```bash
make release-tag-push VERSION=0.2.0
```

## SemVer Interpretation

1. `MAJOR`:
- Breaking changes to documented external behavior.
- Examples:
  - scenario YAML schema break
  - contract format break for Kafka job/result/summary/DLQ payloads
  - metric name/label break without compatibility bridge

2. `MINOR`:
- Backward-compatible feature additions.
- Examples:
  - new optional scenario fields
  - new metrics
  - new adapters/protocol support

3. `PATCH`:
- Backward-compatible fixes.
- Examples:
  - bug fixes
  - performance improvements without contract changes
  - docs/operational hardening updates

## Compatibility Surface

The following are treated as compatibility-sensitive:

- Scenario YAML schema (versioned via `version` field).
- Kafka payload schemas (`ScenarioJob`, `ScenarioRunResult`, `ScenarioRunSummary`,
  revisioned `ScenarioRunSummaryEvent`, `FailedScenarioJob`, and bounded poison-message
  envelopes).
- Environment variables used for runtime configuration.
- Prometheus metric names and core label keys used by dashboards/alerts.

## Current Support Window

- `0.x` line is considered pre-1.0:
  - breaking changes may still occur between minors,
  - but they must be documented in `CHANGELOG.md`.
- At `1.0.0+`, SemVer rules will be enforced strictly for compatibility-sensitive surfaces.

Kafka payloads carry an explicit `schema_version`. The runtime accepts supported old
versions after validation and rejects unknown future versions. Additive fields must
have defined defaults before they can be treated as backward compatible; malformed
required fields and invalid slice/attempt metadata follow the poison/permanent-failure
settlement path rather than being guessed.

Version 1 result records predate mergeable histograms. They remain readable for count
compatibility, but their aggregate percentiles are unavailable (reported as zero) and
must not be used as latency evidence. A run cannot mix result schema versions.

Version 1 jobs also predate the stamped scenario-plan fingerprint and per-slice startup
burst. They are accepted for a bounded migration window but cannot prove worker-plan
equivalence; version 2 producers should be used for distributed review evidence.

Poison envelopes retain their deterministic `event_id` and source coordinates across
evidence revisions. New envelopes include original byte counts and truncation flags for
bounded key/payload prefixes; absent metadata defaults to `None`/`false` when reading
legacy envelopes. Truncated evidence is not a lossless copy of the source record.

Summary consumers must treat `(event_id, revision)` as an idempotency identity. A late
slice can legitimately create a newer revision for the same run; an exact repeated
revision is a delivery duplicate, not a second aggregate.

## Deprecation Policy

- Deprecations are announced in changelog under `Deprecated`.
- Removal target version is stated when deprecation is introduced.
- At least one tagged release should include both old and new behavior when practical.
