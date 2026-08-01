#!/usr/bin/env bash
set -euo pipefail

KUBE_CONTEXT="${KUBE_CONTEXT:-kind-account}"
KUBE_NAMESPACE="${KUBE_NAMESPACE:-pulse-dev}"
SOAK_DURATION_SEC="${SOAK_DURATION_SEC:-1800}"
SOAK_SAMPLE_INTERVAL_SEC="${SOAK_SAMPLE_INTERVAL_SEC:-30}"
SOAK_CHAOS_PLAN="${SOAK_CHAOS_PLAN:-kafka,redis,pulse}"
SOAK_REPORT_DIR="${SOAK_REPORT_DIR:-artifacts/reliability}"
SOAK_MIN_JOBS_RECEIVED="${SOAK_MIN_JOBS_RECEIVED:-1}"
SOAK_MIN_RESULTS_PUBLISHED="${SOAK_MIN_RESULTS_PUBLISHED:-1}"
SOAK_MIN_SOURCE_COMMITS="${SOAK_MIN_SOURCE_COMMITS:-1}"
SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC="${SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC:-120}"
SOAK_POST_FAULT_POLL_INTERVAL_SEC="${SOAK_POST_FAULT_POLL_INTERVAL_SEC:-5}"
SOAK_MIN_POST_FAULT_PROGRESS="${SOAK_MIN_POST_FAULT_PROGRESS:-1}"
SOAK_EVIDENCE_ENABLED="${SOAK_EVIDENCE_ENABLED:-true}"
SOAK_EVIDENCE_CLASS="${SOAK_EVIDENCE_CLASS:-failure_evidence}"
SOAK_BUILD_PROFILE="${SOAK_BUILD_PROFILE:-unknown}"
SOAK_SCENARIO_FILES="${SOAK_SCENARIO_FILES:-scenarios.yaml}"
SOAK_DESCRIPTOR_FILES="${SOAK_DESCRIPTOR_FILES:-descriptors/services.pb}"
SOAK_TARGET_DEPLOYMENT="${SOAK_TARGET_DEPLOYMENT:-}"
SOAK_PULSE_CONFIGMAP="${SOAK_PULSE_CONFIGMAP:-pulse-config}"
SOAK_TARGET_CONFIGMAP="${SOAK_TARGET_CONFIGMAP:-}"
declare -a plan=()

if ! [[ "${SOAK_DURATION_SEC}" =~ ^[0-9]+$ ]] || (( SOAK_DURATION_SEC <= 0 )); then
  echo "SOAK_DURATION_SEC must be a positive integer (got '${SOAK_DURATION_SEC}')" >&2
  exit 1
fi

if ! [[ "${SOAK_SAMPLE_INTERVAL_SEC}" =~ ^[0-9]+$ ]] || (( SOAK_SAMPLE_INTERVAL_SEC <= 0 )); then
  echo "SOAK_SAMPLE_INTERVAL_SEC must be a positive integer (got '${SOAK_SAMPLE_INTERVAL_SEC}')" >&2
  exit 1
fi

for minimum in SOAK_MIN_JOBS_RECEIVED SOAK_MIN_RESULTS_PUBLISHED SOAK_MIN_SOURCE_COMMITS; do
  value="${!minimum}"
  if ! [[ "${value}" =~ ^[0-9]+$ ]]; then
    echo "${minimum} must be a non-negative integer (got '${value}')" >&2
    exit 1
  fi
done

for setting in SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC SOAK_POST_FAULT_POLL_INTERVAL_SEC SOAK_MIN_POST_FAULT_PROGRESS; do
  value="${!setting}"
  if ! [[ "${value}" =~ ^[0-9]+$ ]] || (( value <= 0 )); then
    echo "${setting} must be a positive integer (got '${value}')" >&2
    exit 1
  fi
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required for Prometheus data-plane assertions" >&2
  exit 1
fi

mkdir -p "${SOAK_REPORT_DIR}"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
report_file="${SOAK_REPORT_DIR}/soak-chaos-${timestamp}.log"
evidence_bundle_dir="${SOAK_EVIDENCE_DIR:-${SOAK_REPORT_DIR}/evidence-soak-${timestamp}}"
evidence_started=false
evidence_finalized=false

is_truthy() {
  local raw
  raw="$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')"
  [[ "${raw}" == "1" || "${raw}" == "true" || "${raw}" == "yes" || "${raw}" == "y" ]]
}

finalize_evidence() {
  if [[ "${evidence_started}" != "true" || "${evidence_finalized}" == "true" ]]; then
    return 0
  fi
  mkdir -p "${evidence_bundle_dir}/derived"
  if [[ -f "${report_file}" ]]; then
    cp -- "${report_file}" "${evidence_bundle_dir}/derived/"
  fi
  EVIDENCE_CLASS="${SOAK_EVIDENCE_CLASS}" \
  EVIDENCE_BUILD_PROFILE="${SOAK_BUILD_PROFILE}" \
  EVIDENCE_SCENARIO_FILES="${SOAK_SCENARIO_FILES}" \
  EVIDENCE_DESCRIPTOR_FILES="${SOAK_DESCRIPTOR_FILES}" \
  EVIDENCE_TARGET_DEPLOYMENT="${SOAK_TARGET_DEPLOYMENT}" \
  EVIDENCE_PULSE_CONFIGMAP="${SOAK_PULSE_CONFIGMAP}" \
  EVIDENCE_TARGET_CONFIGMAP="${SOAK_TARGET_CONFIGMAP}" \
    scripts/reliability/capture_evidence_bundle.sh finish "${evidence_bundle_dir}"
  evidence_finalized=true
}

cleanup() {
  finalize_evidence >/dev/null 2>&1 || true
}
trap cleanup EXIT

log() {
  local line="$1"
  printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${line}" | tee -a "${report_file}"
}

snapshot_pods() {
  kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods \
    -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount,PHASE:.status.phase \
    --no-headers 2>&1 | tee -a "${report_file}" >/dev/null
}

prom_query_scalar() {
  local query_label="$1"
  local query="$2"
  local encoded
  local response
  local safe_label
  local raw_file
  safe_label="$(printf '%s' "${query_label}" | tr -cs 'A-Za-z0-9._-' '_')"
  raw_file="${evidence_bundle_dir}/raw-prometheus/${safe_label}.json"
  encoded="$(python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.argv[1], safe=""))' "${query}")"
  response="$(
    kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" exec deploy/prometheus -- \
      sh -lc "wget -qO- 'http://127.0.0.1:9090/api/v1/query?query=${encoded}'"
  )"
  if [[ "${evidence_started}" == "true" ]]; then
    printf '%s\n' "${query}" >"${evidence_bundle_dir}/raw-prometheus/${safe_label}.promql"
    printf '%s\n' "${response}" >"${raw_file}"
    printf '[%s] prometheus_query label=%q promql=%q\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${query_label}" "${query}" \
      >>"${evidence_bundle_dir}/commands/commands.log"
  fi
  python3 -c '
import json, sys
payload = json.load(sys.stdin)
result = payload.get("data", {}).get("result", [])
if payload.get("status") != "success" or not result:
    raise SystemExit("Prometheus query returned no scalar")
print(result[0]["value"][1])
' <<<"${response}"
}

assert_minimum() {
  local name="$1"
  local actual="$2"
  local minimum="$3"
  if awk -v actual="${actual}" -v minimum="${minimum}" 'BEGIN { exit !(actual >= minimum) }'; then
    log "data-plane assertion passed: ${name}=${actual} minimum=${minimum}"
  else
    failures=$((failures + 1))
    log "data-plane assertion FAILED: ${name}=${actual} minimum=${minimum}"
  fi
}

append_timeline() {
  local event="$1"
  local target="$2"
  local outcome="$3"
  local detail="${4:-}"
  if [[ "${evidence_started}" != "true" ]]; then
    return 0
  fi
  python3 - \
    "${evidence_bundle_dir}/failure-timeline.jsonl" \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "${event}" \
    "${target}" \
    "${outcome}" \
    "${detail}" <<'PY'
import json
import sys

path, timestamp, event, target, outcome, detail = sys.argv[1:]
with open(path, "a", encoding="utf-8") as handle:
    handle.write(json.dumps({
        "timestamp_utc": timestamp,
        "event": event,
        "target": target,
        "outcome": outcome,
        "detail": detail or None,
    }, separators=(",", ":")) + "\n")
PY
}

post_fault_progress() {
  local event_id="$1"
  local target="$2"
  local recovered_epoch="$3"
  local deadline="$((SECONDS + SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC))"
  local attempt=0
  local now_epoch
  local elapsed
  local query_window
  local jobs_received
  local results_published
  local source_commits

  while (( SECONDS < deadline )); do
    attempt=$((attempt + 1))
    now_epoch="$(date +%s)"
    elapsed=$((now_epoch - recovered_epoch))
    if (( elapsed < 2 )); then
      sleep "${SOAK_POST_FAULT_POLL_INTERVAL_SEC}"
      continue
    fi
    query_window="${elapsed}s"

    if jobs_received="$(prom_query_scalar \
        "post-fault-${event_id}-${target}-${attempt}-jobs" \
        "sum(increase(pulse_worker_jobs_received_total[${query_window}])) or vector(0)")" \
      && results_published="$(prom_query_scalar \
        "post-fault-${event_id}-${target}-${attempt}-results" \
        "sum(increase(pulse_worker_results_published_total[${query_window}])) or vector(0)")" \
      && source_commits="$(prom_query_scalar \
        "post-fault-${event_id}-${target}-${attempt}-commits" \
        "sum(increase(pulse_worker_job_commits_total[${query_window}])) or vector(0)")"; then
      if awk \
        -v jobs="${jobs_received}" \
        -v results="${results_published}" \
        -v commits="${source_commits}" \
        -v minimum="${SOAK_MIN_POST_FAULT_PROGRESS}" \
        'BEGIN { exit !(jobs >= minimum && results >= minimum && commits >= minimum) }'; then
        log "post-fault data-plane assertion passed: target=${target} window=${query_window} jobs_received=${jobs_received} results_published=${results_published} source_commits=${source_commits} minimum=${SOAK_MIN_POST_FAULT_PROGRESS}"
        append_timeline \
          "post_fault_data_plane_assertion" \
          "${target}" \
          "passed" \
          "window=${query_window},jobs=${jobs_received},results=${results_published},commits=${source_commits}"
        return 0
      fi
      log "waiting for post-fault data-plane progress: target=${target} window=${query_window} jobs_received=${jobs_received} results_published=${results_published} source_commits=${source_commits} minimum=${SOAK_MIN_POST_FAULT_PROGRESS}"
    else
      log "post-fault Prometheus query failed: target=${target} attempt=${attempt}"
    fi
    sleep "${SOAK_POST_FAULT_POLL_INTERVAL_SEC}"
  done

  log "post-fault data-plane assertion FAILED: target=${target} timeout_sec=${SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC}"
  append_timeline \
    "post_fault_data_plane_assertion" \
    "${target}" \
    "failed" \
    "timeout_sec=${SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC}"
  return 1
}

restart_and_wait() {
  local deployment="$1"
  local target="$2"
  local event_id="$3"
  local recovered_epoch
  log "chaos event: rollout restart deployment/${deployment}"
  append_timeline "fault_injection" "${target}" "started" "deployment=${deployment}"
  if ! kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" \
      rollout restart "deployment/${deployment}" | tee -a "${report_file}"; then
    append_timeline "fault_injection" "${target}" "failed" "rollout restart failed"
    return 1
  fi
  if ! kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" \
      rollout status "deployment/${deployment}" --timeout=300s | tee -a "${report_file}"; then
    append_timeline "fault_recovery" "${target}" "failed" "rollout did not become ready"
    return 1
  fi
  log "chaos event completed: deployment/${deployment}"
  append_timeline "fault_recovery" "${target}" "deployment_ready" "deployment=${deployment}"
  recovered_epoch="$(date +%s)"
  post_fault_progress "${event_id}" "${target}" "${recovered_epoch}"
}

split_plan() {
  local raw="$1"
  local -a plan_raw=()
  local trimmed=""
  IFS=',' read -r -a plan_raw <<< "${raw}"
  plan=()
  for item in "${plan_raw[@]}"; do
    trimmed="$(echo "${item}" | xargs)"
    [[ -n "${trimmed}" ]] || continue
    case "${trimmed}" in
      kafka | redis | pulse) plan+=("${trimmed}") ;;
      *)
        echo "unsupported chaos target '${trimmed}' (expected kafka, redis, or pulse)" >&2
        return 1
        ;;
    esac
  done
}

chaos_action() {
  local target="$1"
  local event_id="$2"
  case "${target}" in
    kafka) restart_and_wait "kafka" "${target}" "${event_id}" ;;
    redis) restart_and_wait "redis" "${target}" "${event_id}" ;;
    pulse) restart_and_wait "pulse" "${target}" "${event_id}" ;;
    *) return 1 ;;
  esac
}

if is_truthy "${SOAK_EVIDENCE_ENABLED}"; then
  EVIDENCE_CLASS="${SOAK_EVIDENCE_CLASS}" \
  EVIDENCE_BUILD_PROFILE="${SOAK_BUILD_PROFILE}" \
  EVIDENCE_SCENARIO_FILES="${SOAK_SCENARIO_FILES}" \
  EVIDENCE_DESCRIPTOR_FILES="${SOAK_DESCRIPTOR_FILES}" \
  EVIDENCE_TARGET_DEPLOYMENT="${SOAK_TARGET_DEPLOYMENT}" \
  EVIDENCE_PULSE_CONFIGMAP="${SOAK_PULSE_CONFIGMAP}" \
  EVIDENCE_TARGET_CONFIGMAP="${SOAK_TARGET_CONFIGMAP}" \
    scripts/reliability/capture_evidence_bundle.sh start "${evidence_bundle_dir}"
  evidence_started=true
  printf '[%s] invocation script=%q context=%q namespace=%q duration_sec=%q sample_interval_sec=%q chaos_plan=%q\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$0" "${KUBE_CONTEXT}" "${KUBE_NAMESPACE}" \
    "${SOAK_DURATION_SEC}" "${SOAK_SAMPLE_INTERVAL_SEC}" "${SOAK_CHAOS_PLAN}" \
    >>"${evidence_bundle_dir}/commands/commands.log"
fi

log "starting soak/chaos run"
log "context=${KUBE_CONTEXT} namespace=${KUBE_NAMESPACE} duration_sec=${SOAK_DURATION_SEC} sample_interval_sec=${SOAK_SAMPLE_INTERVAL_SEC} chaos_plan=${SOAK_CHAOS_PLAN}"
log "post_fault_assert_timeout_sec=${SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC} post_fault_poll_interval_sec=${SOAK_POST_FAULT_POLL_INTERVAL_SEC} min_post_fault_progress=${SOAK_MIN_POST_FAULT_PROGRESS}"

split_plan "${SOAK_CHAOS_PLAN}"
chaos_count="${#plan[@]}"

start_ts="$(date +%s)"
end_ts="$((start_ts + SOAK_DURATION_SEC))"
failures=0
append_timeline "soak_window" "pulse" "started" "duration_sec=${SOAK_DURATION_SEC}"

if (( chaos_count > 0 )); then
  chaos_spacing="$((SOAK_DURATION_SEC / (chaos_count + 1)))"
  if (( chaos_spacing <= 0 )); then
    chaos_spacing=1
  fi
else
  chaos_spacing=0
fi

next_sample_ts="${start_ts}"
next_chaos_index=0
next_chaos_ts="$((start_ts + chaos_spacing))"

log "report_file=${report_file}"
log "initial pod snapshot"
snapshot_pods

while :; do
  now="$(date +%s)"
  if (( now >= end_ts )); then
    break
  fi

  if (( chaos_count > 0 && next_chaos_index < chaos_count && now >= next_chaos_ts )); then
    target="${plan[next_chaos_index]}"
    event_id="$((next_chaos_index + 1))"
    if ! chaos_action "${target}" "${event_id}"; then
      failures=$((failures + 1))
      log "chaos event failed: target=${target}"
    fi
    next_chaos_index=$((next_chaos_index + 1))
    next_chaos_ts="$((start_ts + chaos_spacing * (next_chaos_index + 1)))"
    now="$(date +%s)"
  fi

  if (( now >= next_sample_ts )); then
    elapsed="$((now - start_ts))"
    remaining="$((end_ts - now))"
    log "sample tick elapsed_sec=${elapsed} remaining_sec=${remaining}"
    snapshot_pods
    next_sample_ts="$((now + SOAK_SAMPLE_INTERVAL_SEC))"
  fi

  sleep 1
done

log "final pod snapshot"
snapshot_pods
metric_window="${SOAK_DURATION_SEC}s"
jobs_received="$(prom_query_scalar "final-jobs-received" "sum(increase(pulse_worker_jobs_received_total[${metric_window}])) or vector(0)")"
results_published="$(prom_query_scalar "final-results-published" "sum(increase(pulse_worker_results_published_total[${metric_window}])) or vector(0)")"
source_commits="$(prom_query_scalar "final-source-commits" "sum(increase(pulse_worker_job_commits_total[${metric_window}])) or vector(0)")"
dlq_published="$(prom_query_scalar "final-dlq-published" "sum(increase(pulse_worker_dlq_published_total[${metric_window}])) or vector(0)")"
duplicates_suppressed="$(prom_query_scalar "final-duplicates-suppressed" "sum(increase(pulse_worker_jobs_duplicate_total[${metric_window}])) or vector(0)")"
result_publish_failures="$(prom_query_scalar "final-result-publish-failures" "sum(increase(pulse_worker_result_publish_failures_total[${metric_window}])) or vector(0)")"
commit_failures="$(prom_query_scalar "final-commit-failures" "sum(increase(pulse_worker_job_commit_failures_total[${metric_window}])) or vector(0)")"
uncommitted_jobs="$(prom_query_scalar "final-uncommitted-jobs" "sum(pulse_worker_uncommitted_jobs) or vector(0)")"
incomplete_dispatch_slices="$(prom_query_scalar "final-incomplete-dispatch-slices" "sum(pulse_scheduler_incomplete_dispatch_slices) or vector(0)")"
assert_minimum "jobs_received" "${jobs_received}" "${SOAK_MIN_JOBS_RECEIVED}"
assert_minimum "results_published" "${results_published}" "${SOAK_MIN_RESULTS_PUBLISHED}"
assert_minimum "source_commits" "${source_commits}" "${SOAK_MIN_SOURCE_COMMITS}"
log "data-plane observations: dlq_published=${dlq_published} duplicates_suppressed=${duplicates_suppressed} result_publish_failures=${result_publish_failures} commit_failures=${commit_failures} uncommitted_jobs=${uncommitted_jobs} incomplete_dispatch_slices=${incomplete_dispatch_slices}"
if [[ "${evidence_started}" == "true" ]]; then
  python3 - \
    "${evidence_bundle_dir}/metadata/data-plane-summary.json" \
    "${jobs_received}" \
    "${results_published}" \
    "${source_commits}" \
    "${dlq_published}" \
    "${duplicates_suppressed}" \
    "${result_publish_failures}" \
    "${commit_failures}" \
    "${uncommitted_jobs}" \
    "${incomplete_dispatch_slices}" <<'PY'
import json
import sys
from pathlib import Path

names = [
    "jobs_received",
    "results_published",
    "source_commits",
    "dlq_published",
    "duplicates_suppressed",
    "result_publish_failures",
    "commit_failures",
    "uncommitted_jobs",
    "incomplete_dispatch_slices",
]
values = {}
for name, raw in zip(names, sys.argv[2:]):
    try:
        values[name] = float(raw)
    except ValueError:
        values[name] = None

payload = {
    "kind": "chaos_data_plane_observations",
    "observations": values,
    "proves_unique_kafka_identities": False,
    "proves_target_exactly_once": False,
}
Path(sys.argv[1]).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
fi
if (( next_chaos_index != chaos_count )); then
  failures=$((failures + 1))
  log "chaos plan assertion FAILED: planned_events=${chaos_count} triggered_events=${next_chaos_index}"
fi
append_timeline \
  "soak_window" \
  "pulse" \
  "$([[ "${failures}" == "0" ]] && echo passed || echo failed)" \
  "jobs=${jobs_received},results=${results_published},commits=${source_commits},planned_events=${chaos_count},triggered_events=${next_chaos_index}"
log "completed soak/chaos run planned_events=${chaos_count} triggered_events=${next_chaos_index} failures=${failures}"
log "report saved to ${report_file}"
finalize_evidence
if [[ "${evidence_started}" == "true" ]]; then
  log "evidence bundle saved to ${evidence_bundle_dir}"
fi

if (( failures > 0 )); then
  exit 1
fi
