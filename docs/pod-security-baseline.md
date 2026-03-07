# Pod Security Baseline

This document defines the minimum pod security posture for Pulse workloads in Kubernetes.

## Scope

- Applies to runtime workloads in `k8s/base` and all overlays.
- Covers `pulse`, `redis`, `kafka`, `prometheus`, and `grafana` deployments.

## Baseline Expectations

1. Run containers as non-root users.
2. Disable privilege escalation.
3. Drop Linux capabilities unless explicitly required.
4. Use RuntimeDefault seccomp profile.
5. Set explicit CPU/memory requests and limits.
6. Use startup/readiness/liveness probes for long-running services.
7. Keep writable storage minimal and explicit (PVC/emptyDir only when required).
8. Keep network access constrained using NetworkPolicy where applicable.

## Current Implementation Mapping

- `runAsNonRoot`, `runAsUser`, `runAsGroup`, `seccompProfile`:
  - `k8s/base/deployment.yaml`
  - `k8s/base/prometheus.yaml`
  - `k8s/base/grafana.yaml`
- `allowPrivilegeEscalation: false` and capability drops:
  - `k8s/base/deployment.yaml`
  - `k8s/base/prometheus.yaml`
  - `k8s/base/grafana.yaml`
- Resource requests/limits:
  - `k8s/base/deployment.yaml`
  - `k8s/base/redis.yaml`
  - `k8s/base/kafka.yaml`
  - `k8s/base/prometheus.yaml`
  - `k8s/base/grafana.yaml`
- Health probes:
  - `k8s/base/deployment.yaml`
  - `k8s/base/prometheus.yaml`
  - `k8s/base/grafana.yaml`
- Network policy example:
  - `k8s/examples/networkpolicy-pulse.yaml`

## Operational Notes

- `make k8s-apply-networkpolicy-example` applies the sample policy for runtime traffic constraints.
- Any exception to this baseline must be documented in an ADR with explicit risk acceptance.
