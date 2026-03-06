# Reliability Testing

This document defines the repeatable soak/chaos run used to validate scheduler/worker resilience during sustained load.

## Scope

- Continuous load over a fixed window (`SOAK_DURATION_SEC`).
- Planned dependency disruptions during active load:
  - Kafka restart
  - Redis restart
  - Pulse restart
- Periodic pod-health snapshots written to a report artifact.

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

Threshold source:

- `k8s/overlays/kind/performance-thresholds.csv`
- `k8s/overlays/staging/performance-thresholds.csv`
- `k8s/overlays/prod/performance-thresholds.csv`

Output:

- Timestamped report files in `artifacts/reliability/`:
  - text log: `artifacts/reliability/perf-gate-20260304T120500Z.log`
  - structured JSON: `artifacts/reliability/perf-gate-20260304T120500Z.json`
- cumulative local history store:
  - JSONL file: `artifacts/reliability/perf-history.jsonl`
- visual markdown report per run:
  - `artifacts/reliability/perf-report-<timestamp>.md`
  - chart assets `artifacts/reliability/perf-report-<timestamp>-<scenario>-*.svg`

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
2. No target deployment remains unavailable after a chaos restart (`rollout status` succeeds).
3. Pulse keeps processing jobs after each disruption:
   - `pulse_worker_jobs_received_total` increases.
4. Pulse keeps publishing outcomes:
   - `pulse_worker_results_published_total` increases.
5. Duplicate suppression stays active:
   - `pulse_worker_jobs_duplicate_total` can increase, but workers do not crash-loop.
6. `make k8s-check-performance` exits with code `0`.
7. All scenarios in the overlay threshold CSV pass:
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
```

## Notes

- This test validates resilience and recovery, not max throughput.
- Keep `SOAK_SAMPLE_INTERVAL_SEC` between `10-60` to balance signal quality and log size.
