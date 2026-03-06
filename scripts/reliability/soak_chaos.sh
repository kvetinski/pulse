#!/usr/bin/env bash
set -euo pipefail

KUBE_CONTEXT="${KUBE_CONTEXT:-kind-account}"
KUBE_NAMESPACE="${KUBE_NAMESPACE:-pulse-dev}"
SOAK_DURATION_SEC="${SOAK_DURATION_SEC:-1800}"
SOAK_SAMPLE_INTERVAL_SEC="${SOAK_SAMPLE_INTERVAL_SEC:-30}"
SOAK_CHAOS_PLAN="${SOAK_CHAOS_PLAN:-kafka,redis,pulse}"
SOAK_REPORT_DIR="${SOAK_REPORT_DIR:-artifacts/reliability}"
declare -a plan=()

if ! [[ "${SOAK_DURATION_SEC}" =~ ^[0-9]+$ ]] || (( SOAK_DURATION_SEC <= 0 )); then
  echo "SOAK_DURATION_SEC must be a positive integer (got '${SOAK_DURATION_SEC}')" >&2
  exit 1
fi

if ! [[ "${SOAK_SAMPLE_INTERVAL_SEC}" =~ ^[0-9]+$ ]] || (( SOAK_SAMPLE_INTERVAL_SEC <= 0 )); then
  echo "SOAK_SAMPLE_INTERVAL_SEC must be a positive integer (got '${SOAK_SAMPLE_INTERVAL_SEC}')" >&2
  exit 1
fi

mkdir -p "${SOAK_REPORT_DIR}"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
report_file="${SOAK_REPORT_DIR}/soak-chaos-${timestamp}.log"

log() {
  local line="$1"
  printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${line}" | tee -a "${report_file}"
}

snapshot_pods() {
  kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods \
    -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount,PHASE:.status.phase \
    --no-headers 2>&1 | tee -a "${report_file}" >/dev/null
}

restart_and_wait() {
  local deployment="$1"
  log "chaos event: rollout restart deployment/${deployment}"
  kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" rollout restart "deployment/${deployment}" \
    | tee -a "${report_file}"
  kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" rollout status "deployment/${deployment}" --timeout=300s \
    | tee -a "${report_file}"
  log "chaos event completed: deployment/${deployment}"
}

split_plan() {
  local raw="$1"
  local -a plan_raw=()
  local trimmed=""
  IFS=',' read -r -a plan_raw <<< "${raw}"
  plan=()
  for item in "${plan_raw[@]}"; do
    trimmed="$(echo "${item}" | xargs)"
    if [[ -n "${trimmed}" ]]; then
      plan+=("${trimmed}")
    fi
  done
}

chaos_action() {
  local target="$1"
  case "${target}" in
    kafka) restart_and_wait "kafka" ;;
    redis) restart_and_wait "redis" ;;
    pulse) restart_and_wait "pulse" ;;
    *)
      log "unknown chaos target '${target}', skipping"
      ;;
  esac
}

log "starting soak/chaos run"
log "context=${KUBE_CONTEXT} namespace=${KUBE_NAMESPACE} duration_sec=${SOAK_DURATION_SEC} sample_interval_sec=${SOAK_SAMPLE_INTERVAL_SEC} chaos_plan=${SOAK_CHAOS_PLAN}"

split_plan "${SOAK_CHAOS_PLAN}"
chaos_count="${#plan[@]}"

start_ts="$(date +%s)"
end_ts="$((start_ts + SOAK_DURATION_SEC))"
failures=0

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
    if ! chaos_action "${target}"; then
      failures=$((failures + 1))
      log "chaos event failed: target=${target}"
    fi
    next_chaos_index=$((next_chaos_index + 1))
    next_chaos_ts="$((start_ts + chaos_spacing * (next_chaos_index + 1)))"
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
log "completed soak/chaos run planned_events=${chaos_count} triggered_events=${next_chaos_index} failures=${failures}"
log "report saved to ${report_file}"

if (( failures > 0 )); then
  exit 1
fi
