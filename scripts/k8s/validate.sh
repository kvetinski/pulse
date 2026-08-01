#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

command -v kubectl >/dev/null 2>&1 || { echo "kubectl is required" >&2; exit 1; }
render_dir="$(mktemp -d)"
trap 'rm -rf -- "${render_dir}"' EXIT

kubectl kustomize k8s/base >"${render_dir}/base.yaml"
kubectl kustomize k8s/demo >"${render_dir}/demo.yaml"
kubectl kustomize k8s/overlays/kind >"${render_dir}/kind.yaml"
kubectl kustomize k8s/overlays/staging >"${render_dir}/staging.yaml"
kubectl kustomize k8s/overlays/prod >"${render_dir}/prod.yaml"
kubectl kustomize k8s/examples/alerts >"${render_dir}/alerts.yaml"

dependency_names='^  name: (grpc-target|kafka|redis|prometheus|grafana|pulse-metrics-headless)$'
if grep -Eq "${dependency_names}" "${render_dir}/base.yaml"; then
  echo "application base unexpectedly contains a demo dependency" >&2
  exit 1
fi
for overlay in staging prod; do
  if grep -Eq "${dependency_names}" "${render_dir}/${overlay}.yaml"; then
    echo "${overlay} overlay unexpectedly contains a demo dependency" >&2
    exit 1
  fi
  grep -Fq 'PULSE_KAFKA_TOPIC_MANAGEMENT_ENABLED: "false"' \
    "${render_dir}/${overlay}.yaml" \
    || { echo "${overlay} must disable topic management" >&2; exit 1; }
  grep -Fq 'optional: false' "${render_dir}/${overlay}.yaml" \
    || { echo "${overlay} must require the Redis secret" >&2; exit 1; }
done

for dependency in grpc-target kafka redis prometheus grafana pulse-metrics-headless; do
  grep -Eq "^  name: ${dependency}$" "${render_dir}/kind.yaml" \
    || { echo "kind demo is missing ${dependency}" >&2; exit 1; }
done

grep -Fq 'PULSE_ENDPOINT: http://grpc-target:50051' "${render_dir}/kind.yaml" \
  || { echo "kind demo does not target its deterministic gRPC fixture" >&2; exit 1; }
grep -Fq 'PULSE_GRPC_DESCRIPTOR_SET: /app/descriptors/demo.pb' "${render_dir}/kind.yaml" \
  || { echo "kind demo does not use the fixture descriptor" >&2; exit 1; }
grep -Fq 'PULSE_SCENARIOS_FILE: /app/scenarios.kind.yaml' "${render_dir}/kind.yaml" \
  || { echo "kind demo does not use its recurring scenario plan" >&2; exit 1; }
grep -Fq 'name: pulse-metrics-headless' "${render_dir}/kind.yaml" \
  || { echo "kind demo is missing per-replica metrics discovery" >&2; exit 1; }
grep -Fq 'clusterIP: None' "${render_dir}/kind.yaml" \
  || { echo "kind metrics discovery service is not headless" >&2; exit 1; }
grep -Fq 'names: ["pulse-metrics-headless"]' "${render_dir}/kind.yaml" \
  || { echo "kind Prometheus does not discover all Pulse replicas" >&2; exit 1; }
grep -Fq 'COPY k8s/overlays/kind/scenarios.kind.yaml /app/scenarios.kind.yaml' \
  demo/Dockerfile.pulse \
  || { echo "kind scenario plan is not present in the demo Pulse image" >&2; exit 1; }
grep -Fq 'name: KindUnaryHealthySoak' k8s/overlays/kind/scenarios.kind.yaml \
  || { echo "kind demo is missing its healthy recurring scenario" >&2; exit 1; }
grep -Fq 'KindUnaryHealthySoak,' k8s/overlays/kind/performance-thresholds.csv \
  || { echo "kind smoke threshold does not match its healthy scenario" >&2; exit 1; }

grep -Fq 'path: /health/live' "${render_dir}/base.yaml" \
  || { echo "app base is missing the liveness route" >&2; exit 1; }
grep -Fq 'path: /health/ready' "${render_dir}/base.yaml" \
  || { echo "app base is missing the readiness route" >&2; exit 1; }

if grep -ERq 'image: .*:latest([[:space:]]|$)' "${render_dir}"; then
  echo "rendered Kubernetes manifest contains a latest image" >&2
  exit 1
fi

echo "Kubernetes app/demo boundary and all rendered manifests are valid"
