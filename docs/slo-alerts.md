# Pulse SLO and Alert Draft

This document is an experimental alert draft. Values are starting hypotheses, not
production SLO evidence. Target-service success measures the exercised service, while
Pulse settlement health measures the load engine; they must not be conflated.

Concrete `PrometheusRule` manifests are provided per overlay:
- `k8s/examples/alerts/pulse-prometheusrule.kind.yaml`
- `k8s/examples/alerts/pulse-prometheusrule.staging.yaml`
- `k8s/examples/alerts/pulse-prometheusrule.prod.yaml`

Apply (requires `prometheusrules.monitoring.coreos.com` CRD):
```bash
make k8s-apply-prometheusrule K8S_OVERLAY=kind
make k8s-apply-prometheusrule K8S_OVERLAY=staging
make k8s-apply-prometheusrule K8S_OVERLAY=prod
```

Delete:
```bash
make k8s-delete-prometheusrule K8S_OVERLAY=kind
```

## SLIs

1. Scenario success ratio
- Source: `pulse_scenario_executions_total{status="success|failure"}`
- Formula:
  - `success / (success + failure)`

2. Scenario latency p95 (successful runs)
- Source: `pulse_scenario_duration_seconds_bucket{status="success"}`
- Formula:
  - `histogram_quantile(0.95, sum by (le, scenario) (rate(pulse_scenario_duration_seconds_bucket{status="success"}[5m])))`

3. Worker consume health
- Source: `pulse_worker_job_consume_errors_total`, `pulse_worker_jobs_received_total`
- Formula:
  - `rate(consume_errors[5m]) / rate(jobs_received[5m])`

4. DLQ publish rate
- Source: `pulse_worker_dlq_published_total`
- Formula:
  - `sum(rate(pulse_worker_dlq_published_total[5m]))`

5. Terminal settlement health

- Sources: `pulse_worker_result_publish_failures_total`,
  `pulse_worker_dlq_publish_failures_total`, and
  `pulse_worker_job_commit_failures_total`.
- Any sustained non-zero rate means source records may remain unsettled and requires
  investigation alongside Kafka consumer lag.

6. Runtime readiness

- Source: HTTP `GET /health/ready` from the platform probe/blackbox monitor.
- Readiness is false during startup, dependency loss, incomplete initialization, or
  shutdown drain; liveness remains a separate process signal.

7. Coordination and unsettled work

- Sources: `pulse_worker_uncommitted_jobs`,
  `pulse_scheduler_incomplete_dispatch_slices`,
  `pulse_worker_execution_lease_total`, and
  `pulse_scheduler_leadership_renewal_failures_total`.
- A non-zero backlog can be normal briefly, but a value sustained beyond the maximum
  configured work window indicates stalled settlement or dispatch.

8. Aggregate completeness

- Source: `pulse_aggregate_results_total{outcome}`.
- `complete` and `late_complete` are complete revisions; `timed_out` is an incomplete
  deadline revision; `duplicate` confirms duplicate suppression. Use the summary event
  itself for exact received/missing slices rather than introducing a `run_id` label.

## Initial SLO Targets

- Availability SLO: scenario success ratio >= 99.0% over 30 days.
- Latency SLO: scenario p95 <= 2.0s for core scenarios over 30 days.
- Reliability SLO: DLQ publish rate near zero during normal operations.

These values must be tuned from controlled evidence bundles before they become SLOs.

## Suggested Alerts (PrometheusRule style)

## 1) High Scenario Failure Ratio

Expression:
```promql
(
  sum(rate(pulse_scenario_executions_total{status="failure"}[5m]))
/
  clamp_min(sum(rate(pulse_scenario_executions_total[5m])), 1)
) > 0.05
```
For: `10m`
Severity: `critical`

## 2) Scenario Latency p95 Too High

Expression:
```promql
histogram_quantile(
  0.95,
  sum by (le) (rate(pulse_scenario_duration_seconds_bucket{status="success"}[10m]))
) > 2
```
For: `15m`
Severity: `warning`

## 3) No Scheduler Leader

Expression:
```promql
max(pulse_scheduler_is_leader) < 1
```
For: `5m`
Severity: `critical`

## 4) Worker Consume Errors High

Expression:
```promql
(
  rate(pulse_worker_job_consume_errors_total[5m])
/
  clamp_min(rate(pulse_worker_jobs_received_total[5m]), 1)
) > 0.01
```
For: `10m`
Severity: `warning`

## 5) DLQ Activity Detected

Expression:
```promql
sum(rate(pulse_worker_dlq_published_total[5m])) > 0
```
For: `10m`
Severity: `warning`

## 6) DLQ Publish Failures

Expression:
```promql
sum(rate(pulse_worker_dlq_publish_failures_total[5m])) > 0
```
For: `5m`
Severity: `critical`

## 7) Result Publication Failures

Expression:
```promql
sum(rate(pulse_worker_result_publish_failures_total[5m])) > 0
```
For: `5m`
Severity: `critical`

## 8) Source Offset Commit Failures

Expression:
```promql
sum(rate(pulse_worker_job_commit_failures_total[5m])) > 0
```
For: `5m`
Severity: `critical`

## 9) Uncommitted Jobs Stuck

Expression:
```promql
sum(pulse_worker_uncommitted_jobs) > 0
```
For: `5m`
Severity: `critical`

## 10) Incomplete Scheduler Dispatch

Expression:
```promql
sum(pulse_scheduler_incomplete_dispatch_slices) > 0
```
For: `5m`
Severity: `warning`

## 11) Summary Publication Failures

Expression:
```promql
sum(rate(pulse_kafka_publish_duration_seconds_count{kind="summary",outcome="failed"}[5m])) > 0
```
For: `5m`
Severity: `critical`

## 12) Aggregate Timeouts

Expression:
```promql
sum(rate(pulse_aggregate_results_total{outcome="timed_out"}[10m])) > 0
```
For: `10m`
Severity: `warning`

## Remaining Metric Gaps

Pulse now exposes bounded job age, processing time, lease outcomes/recovery, dispatch
incompleteness and lag, retry queue depth, retry job age
(`pulse_worker_retry_job_age_seconds`), Kafka publication/commit latency, shutdown drain
time, and aggregate outcomes. It does not yet expose durable summary-outbox depth/age
or a separate result-consumer uncommitted/commit-failure series. Use consumer-group lag,
Redis outbox inspection, and structured logs for those gaps. Do not add unbounded
`run_id`, execution key, topic offset, or lease-token metric labels.

## Alert Routing Guidance

- `critical`: page on-call immediately.
- `warning`: notify Slack/ops channel, page only if sustained > 30m.
- Always attach runbook link: `docs/runbook.md`.
