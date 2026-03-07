# Compatibility Policy

This document defines release compatibility expectations for Pulse.

## Versioning and Tags

- Release tags use semantic format: `vMAJOR.MINOR.PATCH` (example: `v0.1.0`).
- Source of truth for released changes is `CHANGELOG.md`.
- Tag creation command:

```bash
make release-tag VERSION=0.1.0
```

- Push tag:

```bash
make release-tag-push VERSION=0.1.0
```

## SemVer Interpretation

1. `MAJOR`:
- Breaking changes to documented external behavior.
- Examples:
  - scenario YAML schema break
  - contract format break for Kafka job/result/DLQ payloads
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
- Kafka payload schemas (`ScenarioJob`, `ScenarioRunResult`, `FailedScenarioJob`).
- Environment variables used for runtime configuration.
- Prometheus metric names and core label keys used by dashboards/alerts.

## Current Support Window

- `0.x` line is considered pre-1.0:
  - breaking changes may still occur between minors,
  - but they must be documented in `CHANGELOG.md`.
- At `1.0.0+`, SemVer rules will be enforced strictly for compatibility-sensitive surfaces.

## Deprecation Policy

- Deprecations are announced in changelog under `Deprecated`.
- Removal target version is stated when deprecation is introduced.
- At least one tagged release should include both old and new behavior when practical.
