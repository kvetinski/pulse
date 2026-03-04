# Pulse SLO and Alert Draft

This document proposes initial SLIs/SLOs and Prometheus alert rules for Pulse.

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

## Initial SLO Targets

- Availability SLO: scenario success ratio >= 99.0% over 30 days.
- Latency SLO: scenario p95 <= 2.0s for core scenarios over 30 days.
- Reliability SLO: DLQ publish rate near zero during normal operations.

These are starting values and should be tuned from production baselines.

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

## Alert Routing Guidance

- `critical`: page on-call immediately.
- `warning`: notify Slack/ops channel, page only if sustained > 30m.
- Always attach runbook link: `docs/runbook.md`.
