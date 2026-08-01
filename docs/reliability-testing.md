# Reliability Testing

This document defines the kind/demo soak and chaos workflow. It is evidence only when
the data-plane assertions and evidence-bundle requirements pass; pod restart success by
itself is not a reliability result.

Start the self-contained kind stack first with `make k8s-deploy-kind`. It builds the
Pulse image with the demo descriptor and recurring kind scenario, builds the
repository-owned `grpc-target` image, and loads both into kind. No external gRPC service
is needed. Prometheus discovers all ready Pulse pod IPs through the demo-only headless
metrics service, so multi-replica progress and leadership observations do not depend on
which backend a ClusterIP happened to select.

## Scope

- Continuous load over a fixed window (`SOAK_DURATION_SEC`).
- Recurring `KindUnaryHealthySoak` load plus a low-rate
  `KindUnaryExpectedTargetFailure` measurement against the deterministic target.
- Planned dependency disruptions during active load:
  - Kafka restart
  - Redis restart
  - Pulse restart
- Periodic pod-health snapshots written to a report artifact.
- A before/after Prometheus assertion around every injected fault proving that jobs,
  durable results, and terminal source commits resumed after recovery.
- Final Prometheus assertions that jobs were received, results were durably published,
  and terminal source offsets were committed.
- A hashed, redacted evidence bundle containing the exact fault timeline and raw
  Prometheus responses.

## Run Command

```bash
make k8s-soak-chaos \
  K8S_OVERLAY=kind \
  SOAK_DURATION_SEC=1800 \
  SOAK_SAMPLE_INTERVAL_SEC=30 \
  SOAK_CHAOS_PLAN=kafka,redis,pulse
```

Output:

- A timestamped report file in `artifacts/reliability/`.
- Example: `artifacts/reliability/soak-chaos-20260304T120000Z.log`
- A finalized evidence directory such as
  `artifacts/reliability/evidence-soak-20260304T120000Z/` with
  `bundle-manifest.json`, `bundle-manifest.sha256`, and `files.sha256`.

The command is intentionally limited to `K8S_OVERLAY=kind` because staging and
production-oriented overlays use externally managed dependencies. Configure optional
minimums with `SOAK_MIN_JOBS_RECEIVED`, `SOAK_MIN_RESULTS_PUBLISHED`, and
`SOAK_MIN_SOURCE_COMMITS` (each defaults to `1`).

Evidence capture is enabled by default. The following controls are available:

- `SOAK_EVIDENCE_ENABLED=false` disables it for a disposable development run;
- `SOAK_EVIDENCE_DIR` selects the bundle directory;
- `SOAK_EVIDENCE_CLASS` defaults to `failure_evidence`;
- `SOAK_BUILD_PROFILE` records the build profile;
- `SOAK_SCENARIO_FILES` and `SOAK_DESCRIPTOR_FILES` are colon-separated input paths;
- `SOAK_TARGET_DEPLOYMENT` enables target deployment metadata and log capture;
- `SOAK_PULSE_CONFIGMAP` and `SOAK_TARGET_CONFIGMAP` select redacted configuration
  snapshots; and
- `SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC`, `SOAK_POST_FAULT_POLL_INTERVAL_SEC`, and
  `SOAK_MIN_POST_FAULT_PROGRESS` tune the per-fault recovery assertion.

The Make defaults retain `k8s/overlays/kind/scenarios.kind.yaml`, the exact demo
descriptor exported during `kind-build`, and `Deployment/grpc-target` metadata/logs in
the evidence bundle. If the stack was not built through `kind-build`, a missing local
descriptor is reported explicitly as an evidence limitation.

## Performance Gate (Step 2)

Run runtime threshold checks against Prometheus metrics after soak:

```bash
make k8s-check-performance \
  K8S_OVERLAY=kind \
  PERF_WINDOW=30m
```

Optional Grafana run annotation (commit/tag marker):

```bash
make k8s-check-performance \
  K8S_OVERLAY=kind \
  PERF_WINDOW=30m \
  PERF_GRAFANA_ANNOTATE=true \
  PERF_GRAFANA_URL=http://127.0.0.1:3000 \
  PERF_GRAFANA_USER=admin \
  PERF_GRAFANA_PASSWORD=admin
```

If you use API tokens, prefer:

```bash
PERF_GRAFANA_TOKEN=<token> make k8s-check-performance K8S_OVERLAY=kind PERF_GRAFANA_ANNOTATE=true
```

This writes a Grafana annotation to dashboard UID `pulse-runtime-metrics` with tags:

- `pulse`
- `perf-gate`
- `overlay:<overlay>`
- `status:<PASS|FAIL>`
- `git_sha:<sha>`
- `git_tag:<tag>`

Current smoke/template threshold sources:

- `k8s/overlays/kind/performance-thresholds.csv`
- `k8s/overlays/staging/performance-thresholds.csv`
- `k8s/overlays/prod/performance-thresholds.csv`

The kind CSV checks only `KindUnaryHealthySoak`. The intentionally failing fixture
scenario is visible in failure metrics but excluded from that healthy-path gate. These
thresholds are coarse environment smoke guardrails, not a capacity claim.

Output:

- Timestamped report files in `artifacts/reliability/`:
  - text log: `artifacts/reliability/perf-gate-20260304T120500Z.log`
  - structured JSON: `artifacts/reliability/perf-gate-20260304T120500Z.json`
- cumulative local history store:
  - JSONL file: `artifacts/reliability/perf-history.jsonl`
- visual markdown report per run:
  - `artifacts/reliability/perf-report-<timestamp>.md`
  - chart assets `artifacts/reliability/perf-report-<timestamp>-<scenario>-*.svg`
- a default evidence bundle at `artifacts/reliability/evidence-<timestamp>/`.

Performance evidence controls mirror the soak controls:
`PERF_EVIDENCE_ENABLED`, `PERF_EVIDENCE_DIR`, `PERF_EVIDENCE_CLASS` (default
`environment_smoke_check`), `PERF_BUILD_PROFILE`, colon-separated
`PERF_SCENARIO_FILES`/`PERF_DESCRIPTOR_FILES`, `PERF_TARGET_DEPLOYMENT`, and
`PERF_PULSE_CONFIGMAP`/`PERF_TARGET_CONFIGMAP`.

JSON fields include:

- run metadata: `timestamp_utc`, `overlay`, `kube_context`, `kube_namespace`, `perf_window`
- git metadata: `git.sha`, `git.branch`, `git.tag`
- summary: `status`, `checked`, `failures`
- per-scenario entries:
  - measured values (`success_rate`, `p95_s`, `p99_s`, `error_rate`)
  - threshold values
  - `status` and `reasons`
- each run JSON is appended as one compact line to `perf-history.jsonl` for trend tooling.

Quick local history view:

```bash
wc -l artifacts/reliability/perf-history.jsonl
tail -n 5 artifacts/reliability/perf-history.jsonl | jq .
```

Open latest visual report:

```bash
latest_report="$(ls -1t artifacts/reliability/perf-report-*.md | head -n 1)"
echo "$latest_report"
```

Generate/update the cumulative history page locally:

```bash
python3 scripts/reliability/generate_perf_history_page.py \
  --history-file artifacts/reliability/perf-history.jsonl \
  --output-file artifacts/reliability/performance-history.html \
  --max-points 60
```

CI artifact publishing:

- GitHub Actions `CI` workflow generates fixture perf artifacts on every run.
- Uploaded artifact bundle name: `perf-artifacts-<run_id>-<run_attempt>`.
- Bundle includes:
  - `perf-gate-*.json`
  - `perf-gate-*.log`
  - `perf-report-*.md`
  - `perf-report-*.svg`
  - `performance-history.html`
  - `perf-history-*.svg`
  - `perf-history.jsonl`

## Chaos Plan

`SOAK_CHAOS_PLAN` is a comma-separated list. Supported entries:

- `kafka`
- `redis`
- `pulse`

Events are distributed across the run window (roughly equal spacing).

## Acceptance Criteria

The run is considered healthy when all conditions are true:

1. `make k8s-soak-chaos` exits with code `0`.
2. No demo deployment remains unavailable after a chaos restart (`rollout status`
   succeeds).
3. Every configured chaos target is recognized and every planned event is triggered.
4. For every injected fault, the data plane advances by at least
   `SOAK_MIN_POST_FAULT_PROGRESS` jobs, results, and commits before the recovery
   timeout.
5. The script's final data-plane assertions pass:
   - `increase(pulse_worker_jobs_received_total[window])` meets the configured minimum;
   - `increase(pulse_worker_results_published_total[window])` meets the configured
     minimum; and
   - `increase(pulse_worker_job_commits_total[window])` meets the configured minimum.
6. Duplicate suppression stays active:
   - `pulse_worker_jobs_duplicate_total` can increase, but workers do not crash-loop.
7. `make k8s-check-performance K8S_OVERLAY=kind` exits with code `0`.
8. All scenarios in the kind threshold CSV pass its smoke thresholds:
   - throughput floor
   - p95 ceiling
   - p99 ceiling
   - error-rate ceiling

Suggested PromQL checks over the test window:

```promql
sum(increase(pulse_worker_jobs_received_total[30m]))
sum(increase(pulse_worker_results_published_total[30m]))
sum(increase(pulse_worker_result_publish_failures_total[30m]))
sum(increase(pulse_worker_job_consume_errors_total[30m]))
sum(increase(pulse_worker_job_commits_total[30m]))
sum(increase(pulse_worker_job_commit_failures_total[30m]))
sum(increase(pulse_worker_dlq_publish_failures_total[30m]))
```

For a release-quality failure claim, also retain source/result/DLQ unique identities,
missing slices, duplicate counts, raw query responses, and the exact fault timeline as
defined in `docs/evidence-policy.md`. The shell workflow captures raw metric progress
and a fault timeline, but metric totals alone do not prove unique Kafka identities or
exactly-once target side effects. Export the relevant Kafka records into the bundle for
claims that depend on identity-level accounting.

## Notes

- This test validates basic resilience and recovery, not max throughput or exactly-once
  target execution.
- Keep `SOAK_SAMPLE_INTERVAL_SEC` between `10-60` to balance signal quality and log size.
