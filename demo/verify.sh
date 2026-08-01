#!/usr/bin/env bash
set -euo pipefail

DEMO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${DEMO_DIR}/.." && pwd)"
COMPOSE=(docker compose --project-name pulse-demo --file "${DEMO_DIR}/compose.yaml")
JOBS_TOPIC="pulse.demo.jobs"
RESULT_TOPIC="pulse.demo.results"
SUMMARY_TOPIC="pulse.demo.summaries"
AGGREGATOR_GROUP="pulse-demo-aggregators"
WAIT_SECONDS="${PULSE_DEMO_RESULT_TIMEOUT_SECONDS:-120}"
PULSE_A_PORT="${PULSE_DEMO_PULSE_A_PORT:-29090}"
PULSE_B_PORT="${PULSE_DEMO_PULSE_B_PORT:-29093}"
PULSE_A_URL="http://127.0.0.1:${PULSE_A_PORT}"
PULSE_B_URL="http://127.0.0.1:${PULSE_B_PORT}"
PULSE_URLS=("${PULSE_A_URL}" "${PULSE_B_URL}")
EVIDENCE_DIR="${PULSE_DEMO_EVIDENCE_DIR:-${REPO_ROOT}/artifacts/demo}"
DEADLINE=$((SECONDS + WAIT_SECONDS))
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "${TEMP_DIR}"' EXIT

mkdir -p "${EVIDENCE_DIR}"

wait_for_readiness() {
    local node="$1"
    local url="$2"
    local output="$3"
    local error_log="$4"
    while (( SECONDS < DEADLINE )); do
        if curl --fail --silent --show-error "${url}/health/ready" \
            >"${output}" 2>"${error_log}"; then
            return 0
        fi
        sleep 1
    done
    echo "timed out after ${WAIT_SECONDS}s waiting for ${node} at ${url}/health/ready" >&2
    sed -n '1,80p' "${error_log}" >&2
    return 1
}

capture_available() {
    local topic="$1"
    local output="$2"
    local consumer_log="$3"

    : >"${output}"
    if "${COMPOSE[@]}" exec -T kafka \
        /opt/kafka/bin/kafka-console-consumer.sh \
        --bootstrap-server kafka:9092 \
        --topic "${topic}" \
        --from-beginning \
        --timeout-ms 5000 \
        >"${output}" 2>"${consumer_log}"; then
        :
    fi
}

wait_for_logical_records() {
    local topic="$1"
    local expected="$2"
    local identity_field="$3"
    local record_name="$4"
    local physical_output="$5"
    local logical_output="$6"
    local consumer_log="$7"
    local selection_log="$8"

    while (( SECONDS < DEADLINE )); do
        capture_available "${topic}" "${physical_output}" "${consumer_log}"
        if python3 "${DEMO_DIR}/select_logical_records.py" \
            "${physical_output}" "${logical_output}" "${expected}" \
            "${identity_field}" "${record_name}" \
            >"${selection_log}" 2>&1; then
            cat "${selection_log}"
            return 0
        else
            local status=$?
            if [[ "${status}" -ne 1 ]]; then
                cat "${selection_log}" >&2
                return "${status}"
            fi
        fi
        sleep 1
    done
    echo "timed out after ${WAIT_SECONDS}s waiting for ${expected} logical ${record_name} on ${topic}" >&2
    sed -n '1,120p' "${consumer_log}" >&2
    sed -n '1,120p' "${selection_log}" >&2
    return 1
}

duplicate_metric() {
    local url
    for url in "${PULSE_URLS[@]}"; do
        curl --fail --silent --show-error "${url}/metrics"
    done | awk 'BEGIN { value = 0 }
            $1 == "pulse_aggregate_results_total{outcome=\"duplicate\"}" { value += $2 }
            END { print int(value) }'
}

aggregator_caught_up() {
    local snapshot="$1"
    local error_log="$2"
    if ! "${COMPOSE[@]}" exec -T kafka \
        /opt/kafka/bin/kafka-consumer-groups.sh \
        --bootstrap-server kafka:9092 \
        --group "${AGGREGATOR_GROUP}" \
        --describe \
        >"${snapshot}" 2>"${error_log}"; then
        return 1
    fi
    python3 "${DEMO_DIR}/check_consumer_group.py" "${snapshot}" "${RESULT_TOPIC}"
}

wait_for_aggregator_catch_up() {
    local stage="$1"
    while (( SECONDS < DEADLINE )); do
        if aggregator_caught_up \
            "${TEMP_DIR}/aggregator-group.txt" \
            "${TEMP_DIR}/aggregator-group.log"; then
            return 0
        fi
        sleep 1
    done
    echo "aggregator group did not catch up ${stage}" >&2
    sed -n '1,120p' "${TEMP_DIR}/aggregator-group.log" >&2
    return 1
}

echo
echo "VERIFY     durable contracts and cluster-wide invariants"
echo "------------------------------------------------------------"
echo "READY      checking both Pulse replicas..."
wait_for_readiness \
    "pulse-demo-a" \
    "${PULSE_A_URL}" \
    "${TEMP_DIR}/readiness-pulse-a.txt" \
    "${TEMP_DIR}/readiness-pulse-a.log"
wait_for_readiness \
    "pulse-demo-b" \
    "${PULSE_B_URL}" \
    "${TEMP_DIR}/readiness-pulse-b.txt" \
    "${TEMP_DIR}/readiness-pulse-b.log"
echo "READY      pulse-demo-a + pulse-demo-b + Kafka + Redis + target [ok]"

echo "OBSERVE    reading deterministic dispatch records from Kafka..."
wait_for_logical_records \
    "${JOBS_TOPIC}" 3 execution_key jobs \
    "${TEMP_DIR}/jobs-physical.jsonl" \
    "${TEMP_DIR}/jobs.jsonl" \
    "${TEMP_DIR}/jobs-consumer.log" \
    "${TEMP_DIR}/jobs-selection.log"
wait_for_logical_records \
    "${RESULT_TOPIC}" 3 event_id results \
    "${TEMP_DIR}/results-before-physical.jsonl" \
    "${TEMP_DIR}/results.jsonl" \
    "${TEMP_DIR}/results-consumer.log" \
    "${TEMP_DIR}/results-selection.log"
wait_for_logical_records \
    "${SUMMARY_TOPIC}" 2 event_id summaries \
    "${TEMP_DIR}/summaries-before-physical.jsonl" \
    "${TEMP_DIR}/summaries-before.jsonl" \
    "${TEMP_DIR}/summaries-consumer.log" \
    "${TEMP_DIR}/summaries-selection.log"
python3 "${DEMO_DIR}/verify_story.py" \
    "${TEMP_DIR}/jobs.jsonl" \
    "${TEMP_DIR}/results.jsonl" \
    "${TEMP_DIR}/summaries-before.jsonl" \
    "${EVIDENCE_DIR}/events.jsonl"

wait_for_aggregator_catch_up "before duplicate-result injection"
DUPLICATES_BEFORE="$(duplicate_metric)"
echo "AGGREGATE  result-input duplicates before injection=${DUPLICATES_BEFORE}"
echo "RECOVERY   publishing one exact duplicate result event (new Kafka record, same event_id)..."
sed -n '1p' "${TEMP_DIR}/results.jsonl" >"${TEMP_DIR}/replayed-result.json"
"${COMPOSE[@]}" exec -T kafka \
    /opt/kafka/bin/kafka-console-producer.sh \
    --bootstrap-server kafka:9092 \
    --topic "${RESULT_TOPIC}" \
    <"${TEMP_DIR}/replayed-result.json" \
    >"${TEMP_DIR}/producer.log" 2>&1

INJECTION_VERIFIED=false
while (( SECONDS < DEADLINE )); do
    if aggregator_caught_up \
        "${TEMP_DIR}/aggregator-group.txt" \
        "${TEMP_DIR}/aggregator-group.log" \
        && [[ "$(duplicate_metric)" -gt "${DUPLICATES_BEFORE}" ]]; then
        INJECTION_VERIFIED=true
        break
    fi
    sleep 1
done
DUPLICATES_AFTER="$(duplicate_metric)"
if [[ "${INJECTION_VERIFIED}" != "true" ]]; then
    echo "aggregator did not commit the injected duplicate and increment its counter" >&2
    sed -n '1,120p' "${TEMP_DIR}/aggregator-group.log" >&2
    exit 1
fi
echo "RECOVERY   aggregate duplicate counter ${DUPLICATES_BEFORE} -> ${DUPLICATES_AFTER} [ok]"

# The demo aggregator scans its durable outbox every 500 ms. Observe several
# complete scan intervals before taking the final Kafka snapshot so a delayed,
# erroneous revision cannot hide behind asynchronous publication.
echo "OBSERVE    holding a 2s aggregate quiet window before final snapshot..."
sleep 2
wait_for_aggregator_catch_up "after duplicate-result quiet window"

capture_available "${SUMMARY_TOPIC}" \
    "${TEMP_DIR}/summaries-after-physical.jsonl" \
    "${TEMP_DIR}/summaries-after-consumer.log"
capture_available "${RESULT_TOPIC}" \
    "${TEMP_DIR}/results-after-physical.jsonl" \
    "${TEMP_DIR}/results-after-consumer.log"
python3 "${DEMO_DIR}/select_logical_records.py" \
    "${TEMP_DIR}/results-after-physical.jsonl" \
    "${TEMP_DIR}/results-after.jsonl" \
    3 event_id results
python3 "${DEMO_DIR}/verify_replay.py" \
    "${TEMP_DIR}/summaries-before-physical.jsonl" \
    "${TEMP_DIR}/summaries-after-physical.jsonl"

curl --fail --silent --show-error "${PULSE_A_URL}/metrics" \
    >"${TEMP_DIR}/metrics-pulse-a.prom"
curl --fail --silent --show-error "${PULSE_B_URL}/metrics" \
    >"${TEMP_DIR}/metrics-pulse-b.prom"
python3 "${DEMO_DIR}/verify_metrics.py" \
    "pulse-demo-a=${TEMP_DIR}/metrics-pulse-a.prom" \
    "pulse-demo-b=${TEMP_DIR}/metrics-pulse-b.prom"

cp "${TEMP_DIR}/readiness-pulse-a.txt" "${EVIDENCE_DIR}/readiness.txt"
cp "${TEMP_DIR}/readiness-pulse-a.txt" "${EVIDENCE_DIR}/readiness-pulse-a.txt"
cp "${TEMP_DIR}/readiness-pulse-b.txt" "${EVIDENCE_DIR}/readiness-pulse-b.txt"
cp "${TEMP_DIR}/jobs.jsonl" "${EVIDENCE_DIR}/jobs.jsonl"
cp "${TEMP_DIR}/jobs-physical.jsonl" "${EVIDENCE_DIR}/jobs-physical.jsonl"
cp "${TEMP_DIR}/results.jsonl" "${EVIDENCE_DIR}/results.jsonl"
cp "${TEMP_DIR}/results-before-physical.jsonl" \
    "${EVIDENCE_DIR}/results-before-physical.jsonl"
cp "${TEMP_DIR}/results-after-physical.jsonl" \
    "${EVIDENCE_DIR}/results-after-physical.jsonl"
cp "${TEMP_DIR}/summaries-before.jsonl" "${EVIDENCE_DIR}/summaries.jsonl"
cp "${TEMP_DIR}/summaries-before-physical.jsonl" \
    "${EVIDENCE_DIR}/summaries-physical.jsonl"
cp "${TEMP_DIR}/summaries-after-physical.jsonl" \
    "${EVIDENCE_DIR}/summaries-after-physical.jsonl"
cp "${TEMP_DIR}/metrics-pulse-a.prom" "${EVIDENCE_DIR}/metrics-pulse-a.prom"
cp "${TEMP_DIR}/metrics-pulse-b.prom" "${EVIDENCE_DIR}/metrics-pulse-b.prom"
cp "${TEMP_DIR}/aggregator-group.txt" "${EVIDENCE_DIR}/aggregator-group.txt"
cp "${TEMP_DIR}/producer.log" "${EVIDENCE_DIR}/duplicate-producer.log"
printf 'before=%s\nafter=%s\n' "${DUPLICATES_BEFORE}" "${DUPLICATES_AFTER}" \
    >"${EVIDENCE_DIR}/duplicate-counters.txt"

echo "PASS       real target traffic, durable settlement and failure semantics verified"
