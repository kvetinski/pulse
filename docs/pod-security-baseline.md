# Pod Security Baseline

This document defines the minimum pod security posture for Pulse workloads in Kubernetes.

## Scope

- Applies normatively to the Pulse application in `k8s/base` and all overlays.
- `k8s/demo` contains a disposable gRPC target, Kafka, Redis, Prometheus, and Grafana
  fixtures. They are reviewed separately and are not a production security baseline.

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
  - `k8s/demo/grpc-target.yaml`
  - `k8s/demo/prometheus.yaml`
  - `k8s/demo/grafana.yaml`
- `allowPrivilegeEscalation: false` and capability drops:
  - `k8s/base/deployment.yaml`
  - `k8s/demo/grpc-target.yaml`
  - `k8s/demo/prometheus.yaml`
  - `k8s/demo/grafana.yaml`
- Resource requests/limits:
  - `k8s/base/deployment.yaml`
  - `k8s/demo/grpc-target.yaml`
  - `k8s/demo/redis.yaml`
  - `k8s/demo/kafka.yaml`
  - `k8s/demo/prometheus.yaml`
  - `k8s/demo/grafana.yaml`
- Health probes:
  - `k8s/base/deployment.yaml`
  - `k8s/demo/grpc-target.yaml`
  - `k8s/demo/prometheus.yaml`
  - `k8s/demo/grafana.yaml`
- Network policy example:
  - `k8s/examples/networkpolicy-pulse.yaml`

## Operational Notes

- `make k8s-apply-networkpolicy-example` applies the sample policy for runtime traffic constraints.
- The demo Kafka/Redis images are single-node fixtures and do not currently satisfy all
  controls expected of a managed restricted-production workload. They are excluded
  from staging/prod renders.
- Any exception to this baseline must be documented in an ADR with explicit risk acceptance.
