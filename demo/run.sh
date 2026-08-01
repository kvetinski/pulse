#!/usr/bin/env bash
set -euo pipefail

DEMO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${DEMO_DIR}/.." && pwd)"
COMPOSE=(docker compose --project-name pulse-demo --file "${DEMO_DIR}/compose.yaml")
EVIDENCE_DIR="${PULSE_DEMO_EVIDENCE_DIR:-${REPO_ROOT}/artifacts/demo}"
BOOTSTRAP_LOG="${EVIDENCE_DIR}/bootstrap.log"
STARTUP_LOG="${EVIDENCE_DIR}/startup.log"
PULSE_DOCKER_SUBNET="${PULSE_DOCKER_SUBNET:-$(
    "${REPO_ROOT}/scripts/docker/select_subnet.py" \
        --network-name pulse-demo_default
)}"
export PULSE_DOCKER_SUBNET

mkdir -p "${EVIDENCE_DIR}"
for evidence_file in \
    bootstrap.log startup.log runtime.log events.jsonl status.txt \
    readiness.txt readiness-pulse-a.txt readiness-pulse-b.txt \
    jobs.jsonl jobs-physical.jsonl results.jsonl \
    results-before-physical.jsonl results-after-physical.jsonl \
    summaries.jsonl summaries-physical.jsonl summaries-after-physical.jsonl \
    metrics-pulse-a.prom metrics-pulse-b.prom aggregator-group.txt \
    duplicate-producer.log duplicate-counters.txt; do
    : >"${EVIDENCE_DIR}/${evidence_file}"
done
# Remove legacy names so a failed current run cannot be mistaken for old proof.
rm -f -- \
    "${EVIDENCE_DIR}/readiness.json" \
    "${EVIDENCE_DIR}/readiness-pulse-a.json" \
    "${EVIDENCE_DIR}/readiness-pulse-b.json" \
    "${EVIDENCE_DIR}/metrics.prom"
printf 'in-progress\n' >"${EVIDENCE_DIR}/status.txt"

run_quiet() {
    local description="$1"
    local log_file="$2"
    shift 2

    echo "PREPARE    ${description}"
    "$@" >>"${log_file}" 2>&1 &
    local command_pid=$!
    local elapsed=0
    while kill -0 "${command_pid}" 2>/dev/null; do
        sleep 5
        elapsed=$((elapsed + 5))
        if kill -0 "${command_pid}" 2>/dev/null; then
            echo "           still working (${elapsed}s); raw output -> ${log_file}"
        fi
    done
    if wait "${command_pid}"; then
        echo "PREPARE    ${description} [ok]"
        return 0
    fi

    echo "PREPARE    ${description} [failed]" >&2
    tail -120 "${log_file}" >&2 || true
    return 1
}

on_error() {
    printf 'failed\n' >"${EVIDENCE_DIR}/status.txt"
    echo >&2
    echo "Pulse demo failed. Captured setup output:" >&2
    echo "  ${BOOTSTRAP_LOG}" >&2
    echo "  ${STARTUP_LOG}" >&2
    echo "Recent service logs follow:" >&2
    "${COMPOSE[@]}" logs --no-color --tail 160 pulse-a pulse-b grpc-target kafka redis >&2 || true
}
trap on_error ERR

echo
echo "Pulse reviewer demo: real behavior, not container setup output"
echo "Raw Docker output is captured under ${EVIDENCE_DIR}."

# The fixed project name confines cleanup to containers, networks and volumes
# created by this demo, and gives every once-only schedule a clean Redis/Kafka
# state. Raw teardown noise is not part of the reviewer story.
"${COMPOSE[@]}" down --volumes --remove-orphans >>"${BOOTSTRAP_LOG}" 2>&1 || true

run_quiet \
    "building the Pulse and deterministic gRPC fixture images" \
    "${BOOTSTRAP_LOG}" \
    "${COMPOSE[@]}" build

run_quiet \
    "starting isolated Kafka, Redis and the gRPC target" \
    "${STARTUP_LOG}" \
    "${COMPOSE[@]}" up --detach --wait kafka redis grpc-target

run_quiet \
    "starting two Pulse replicas" \
    "${STARTUP_LOG}" \
    "${COMPOSE[@]}" up --detach --wait pulse-a pulse-b

python3 "${DEMO_DIR}/visualize.py" \
    --compose-file "${DEMO_DIR}/compose.yaml" \
    --project-name pulse-demo \
    --evidence-dir "${EVIDENCE_DIR}" \
    --expected-terminals 3 \
    --timeout-seconds "${PULSE_DEMO_RESULT_TIMEOUT_SECONDS:-120}"

run_quiet \
    "starting minimal metrics collection" \
    "${STARTUP_LOG}" \
    "${COMPOSE[@]}" up --detach prometheus

PULSE_DEMO_EVIDENCE_DIR="${EVIDENCE_DIR}" "${DEMO_DIR}/verify.sh"
printf 'complete\n' >"${EVIDENCE_DIR}/status.txt"

trap - ERR
cat <<EOF

Pulse local demo completed successfully.

  Pulse A readiness: http://127.0.0.1:${PULSE_DEMO_PULSE_A_PORT:-29090}/health/ready
  Pulse B readiness: http://127.0.0.1:${PULSE_DEMO_PULSE_B_PORT:-29093}/health/ready
  Pulse A metrics:   http://127.0.0.1:${PULSE_DEMO_PULSE_A_PORT:-29090}/metrics
  Pulse B metrics:   http://127.0.0.1:${PULSE_DEMO_PULSE_B_PORT:-29093}/metrics
  Prometheus:        http://127.0.0.1:${PULSE_DEMO_PROMETHEUS_PORT:-29091}
  Evidence:          ${EVIDENCE_DIR}

The demo stack remains running for inspection. Stop it with: make demo-down
EOF
