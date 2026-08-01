#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

required_targets=(
  doctor validate-config demo demo-down ci k8s-validate test-integration-compose
  supply-chain-check release-validate release-tag
)
for target in "${required_targets[@]}"; do
  grep -Eq "^${target}([[:space:]]|:)" Makefile \
    || { echo "README command has no Make target: ${target}" >&2; exit 1; }
done

required_paths=(
  demo/run.sh demo/down.sh demo/scenarios.yaml demo/verify_story.py
  demo/verify_metrics.py demo/verify_replay.py demo/visualize.py
  demo/check_consumer_group.py demo/select_logical_records.py
  scripts/docker/select_subnet.py scripts/docker/tests/test_select_subnet.py
  demo/tests/test_consumer_group.py demo/tests/test_visualize.py
  demo/tests/test_logical_records.py demo/tests/test_metrics_verifier.py
  docs/configuration.md
  docs/operational-safety.md docs/runbook.md docs/dlq-operations.md
  docs/evidence-policy.md docs/reliability-testing.md docs/compatibility.md
  docs/adr/ADR-0007-failure-model-and-delivery-semantics.md k8s/README.md
)
for path in "${required_paths[@]}"; do
  [[ -f "$path" ]] || { echo "documented path is missing: ${path}" >&2; exit 1; }
done

grep -Fq "At-least-once target execution with deterministic job identities" README.md \
  || { echo "README delivery guarantee is missing" >&2; exit 1; }
grep -Fq '/health/live' k8s/base/deployment.yaml \
  || { echo "Kubernetes liveness probe is not documented health behavior" >&2; exit 1; }
grep -Fq '/health/ready' k8s/base/deployment.yaml \
  || { echo "Kubernetes readiness probe is not documented health behavior" >&2; exit 1; }
cmp --silent ops/grafana/dashboards/pulse-runtime-dashboard.json \
  k8s/demo/dashboards/pulse-runtime-dashboard.json \
  || { echo "Compose and kind demo Grafana dashboards have drifted" >&2; exit 1; }

runtime_env="$(mktemp)"
documented_env="$(mktemp)"
python_cache="$(mktemp -d)"
trap 'rm -f -- "${runtime_env}" "${documented_env}"; rm -rf -- "${python_cache}"' EXIT
grep -oE 'PULSE_[A-Z0-9_]+' src/infrastructure/config/mod.rs | sort -u >"${runtime_env}"
grep -oE 'PULSE_[A-Z0-9_]+' docs/configuration.md | sort -u >"${documented_env}"
missing_env="$(comm -23 "${runtime_env}" "${documented_env}")"
if [[ -n "${missing_env}" ]]; then
  echo "docs/configuration.md is missing runtime environment variables:" >&2
  printf '%s\n' "${missing_env}" >&2
  exit 1
fi

find demo scripts -type f -name '*.sh' -print0 | xargs -0 bash -n
PYTHONPYCACHEPREFIX="${python_cache}" \
  python3 -m unittest discover -s demo/tests -p 'test_*.py'
PYTHONPYCACHEPREFIX="${python_cache}" \
  python3 -m unittest discover -s scripts/docker/tests -p 'test_*.py'
docker compose --project-name pulse-demo --file demo/compose.yaml config --quiet

echo "documented commands, paths, probes, and demo Compose configuration are valid"
