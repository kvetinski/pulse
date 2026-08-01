#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export LANG=C

if (( $# != 2 )); then
  echo "usage: $0 <start|finish> <bundle-directory>" >&2
  exit 2
fi

phase="$1"
bundle_dir="$2"
if [[ "${phase}" != "start" && "${phase}" != "finish" ]]; then
  echo "phase must be 'start' or 'finish' (got '${phase}')" >&2
  exit 2
fi
if [[ -z "${bundle_dir}" || "${bundle_dir}" == "/" || "${bundle_dir}" == "." || "${bundle_dir}" == ".." ]]; then
  echo "bundle directory must be a dedicated non-root path" >&2
  exit 2
fi
if [[ -L "${bundle_dir}" ]]; then
  echo "bundle directory must not be a symbolic link" >&2
  exit 2
fi

bundle_marker="${bundle_dir}/.pulse-evidence-bundle"
if [[ "${phase}" == "start" ]]; then
  if [[ -e "${bundle_dir}" && -n "$(find "${bundle_dir}" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
    echo "refusing to overwrite non-empty evidence directory: ${bundle_dir}" >&2
    exit 2
  fi
elif [[ ! -f "${bundle_marker}" ]]; then
  echo "evidence bundle was not initialized by the start phase: ${bundle_dir}" >&2
  exit 2
fi

KUBE_CONTEXT="${KUBE_CONTEXT:-kind-account}"
KUBE_NAMESPACE="${KUBE_NAMESPACE:-pulse-dev}"
EVIDENCE_CLASS="${EVIDENCE_CLASS:-local_observation}"
EVIDENCE_BUILD_PROFILE="${EVIDENCE_BUILD_PROFILE:-unknown}"
EVIDENCE_PULSE_DEPLOYMENT="${EVIDENCE_PULSE_DEPLOYMENT:-pulse}"
EVIDENCE_PULSE_SELECTOR="${EVIDENCE_PULSE_SELECTOR:-app=pulse}"
EVIDENCE_PULSE_CONFIGMAP="${EVIDENCE_PULSE_CONFIGMAP:-pulse-config}"
EVIDENCE_KAFKA_DEPLOYMENT="${EVIDENCE_KAFKA_DEPLOYMENT:-kafka}"
EVIDENCE_REDIS_DEPLOYMENT="${EVIDENCE_REDIS_DEPLOYMENT:-redis}"
EVIDENCE_TARGET_DEPLOYMENT="${EVIDENCE_TARGET_DEPLOYMENT:-}"
EVIDENCE_TARGET_CONFIGMAP="${EVIDENCE_TARGET_CONFIGMAP:-}"
EVIDENCE_LOG_TAIL_LINES="${EVIDENCE_LOG_TAIL_LINES:-5000}"
EVIDENCE_SCENARIO_FILES="${EVIDENCE_SCENARIO_FILES:-scenarios.yaml}"
EVIDENCE_DESCRIPTOR_FILES="${EVIDENCE_DESCRIPTOR_FILES:-descriptors/services.pb}"

if ! [[ "${EVIDENCE_LOG_TAIL_LINES}" =~ ^[0-9]+$ ]] || (( EVIDENCE_LOG_TAIL_LINES == 0 )); then
  echo "EVIDENCE_LOG_TAIL_LINES must be a positive integer" >&2
  exit 2
fi

metadata_dir="${bundle_dir}/metadata"
config_dir="${bundle_dir}/config"
inputs_dir="${bundle_dir}/inputs"
resources_dir="${bundle_dir}/resources"
logs_dir="${bundle_dir}/logs"
commands_dir="${bundle_dir}/commands"
raw_prometheus_dir="${bundle_dir}/raw-prometheus"
mkdir -p \
  "${metadata_dir}" \
  "${config_dir}" \
  "${inputs_dir}" \
  "${resources_dir}" \
  "${logs_dir}" \
  "${commands_dir}" \
  "${raw_prometheus_dir}"
if [[ "${phase}" == "start" ]]; then
  printf 'pulse-evidence-bundle-v1\n' >"${bundle_marker}"
fi

limitations_file="${bundle_dir}/limitations.txt"
commands_file="${commands_dir}/commands.log"
touch "${limitations_file}" "${commands_file}" "${bundle_dir}/failure-timeline.jsonl"

record_command() {
  printf '[%s] %q' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" >>"${commands_file}"
  shift
  if (( $# > 0 )); then
    printf ' %q' "$@" >>"${commands_file}"
  fi
  printf '\n' >>"${commands_file}"
}

record_limitation() {
  local message="$1"
  if ! grep -Fqx -- "${message}" "${limitations_file}"; then
    printf '%s\n' "${message}" >>"${limitations_file}"
  fi
}

capture_command() {
  local output_file="$1"
  local description="$2"
  shift 2
  record_command "$@"
  if ! "$@" >"${output_file}" 2>"${output_file}.stderr"; then
    record_limitation "${description} unavailable; see ${output_file#${bundle_dir}/}.stderr"
    return 0
  fi
  if [[ ! -s "${output_file}.stderr" ]]; then
    rm -f -- "${output_file}.stderr"
  fi
}

redact_json_file() {
  local source_file="$1"
  local destination_file="$2"
  python3 - "${source_file}" "${destination_file}" <<'PY'
import json
import re
import sys
from pathlib import Path

source = Path(sys.argv[1])
destination = Path(sys.argv[2])
sensitive = re.compile(r"(?:password|passwd|token|secret|credential|private|sasl|api[_-]?key|auth)", re.I)
uri_userinfo = re.compile(r"(://)[^/@\s]+@")

def redact(value, parent_key=""):
    if isinstance(value, dict):
        env_name = str(value.get("name", ""))
        output = {}
        for key, item in value.items():
            if key == "value" and sensitive.search(env_name):
                output[key] = "<redacted>"
            elif sensitive.search(str(key)) and key not in {"secretKeyRef", "secretRef"}:
                output[key] = "<redacted>"
            else:
                output[key] = redact(item, str(key))
        return output
    if isinstance(value, list):
        return [redact(item, parent_key) for item in value]
    if isinstance(value, str):
        return uri_userinfo.sub(r"\1<redacted>@", value)
    return value

payload = json.loads(source.read_text(encoding="utf-8"))
destination.write_text(json.dumps(redact(payload), indent=2, sort_keys=True) + "\n", encoding="utf-8")
PY
}

capture_redacted_deployment() {
  local deployment="$1"
  local label="$2"
  local raw_file
  local destination="${config_dir}/${label}-deployment.json"
  if [[ -z "${deployment}" ]]; then
    record_limitation "${label} deployment was not configured for evidence capture"
    return 0
  fi
  raw_file="$(mktemp)"
  record_command kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get deployment "${deployment}" -o json
  if kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" \
      get deployment "${deployment}" -o json >"${raw_file}" 2>"${destination}.stderr"; then
    redact_json_file "${raw_file}" "${destination}"
    if [[ ! -s "${destination}.stderr" ]]; then
      rm -f -- "${destination}.stderr"
    fi
  else
    record_limitation "${label} deployment '${deployment}' unavailable; see ${destination#${bundle_dir}/}.stderr"
  fi
  rm -f -- "${raw_file}"
}

capture_redacted_configmap() {
  local configmap="$1"
  local label="$2"
  local raw_file
  local destination="${config_dir}/${label}-configmap.json"
  if [[ -z "${configmap}" ]]; then
    record_limitation "${label} ConfigMap was not configured for evidence capture"
    return 0
  fi
  raw_file="$(mktemp)"
  record_command kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get configmap "${configmap}" -o json
  if kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" \
      get configmap "${configmap}" -o json >"${raw_file}" 2>"${destination}.stderr"; then
    redact_json_file "${raw_file}" "${destination}"
    if [[ ! -s "${destination}.stderr" ]]; then
      rm -f -- "${destination}.stderr"
    fi
  else
    record_limitation "${label} ConfigMap '${configmap}' unavailable; see ${destination#${bundle_dir}/}.stderr"
  fi
  rm -f -- "${raw_file}"
}

capture_input_group() {
  local kind="$1"
  local raw_paths="$2"
  local found=0
  local path
  local path_id
  local file_name
  local destination
  local hashes_file="${inputs_dir}/${kind}-sha256.txt"
  : >"${hashes_file}"
  IFS=':' read -r -a paths <<<"${raw_paths}"
  for path in "${paths[@]}"; do
    [[ -n "${path}" ]] || continue
    if [[ ! -f "${path}" ]]; then
      printf 'unavailable  %s\n' "${path}" >>"${hashes_file}"
      continue
    fi
    found=$((found + 1))
    path_id="$(printf '%s' "${path}" | sha256sum | cut -c1-12)"
    file_name="$(basename -- "${path}" | tr -cs 'A-Za-z0-9._-' '_')"
    destination="${inputs_dir}/${kind}-${path_id}-${file_name}"
    cp -- "${path}" "${destination}"
    sha256sum -- "${destination}" >>"${hashes_file}"
  done
  if (( found == 0 )); then
    record_limitation "no ${kind} input file was available from configured paths: ${raw_paths}"
  fi
}

write_window_metadata() {
  local start_file="${metadata_dir}/window-start-utc.txt"
  local end_file="${metadata_dir}/window-end-utc.txt"
  if [[ ! -s "${start_file}" ]]; then
    date -u +%Y-%m-%dT%H:%M:%SZ >"${start_file}"
  fi
  if [[ "${phase}" == "finish" ]]; then
    date -u +%Y-%m-%dT%H:%M:%SZ >"${end_file}"
  fi
  python3 - "${start_file}" "${end_file}" "${metadata_dir}/window.json" <<'PY'
import json
import sys
from pathlib import Path

start_path, end_path, output_path = map(Path, sys.argv[1:])
payload = {
    "start_utc": start_path.read_text(encoding="utf-8").strip(),
    "end_utc": end_path.read_text(encoding="utf-8").strip() if end_path.exists() else None,
}
output_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
}

write_manifest() {
  local file_hashes="${bundle_dir}/files.sha256"
  (
    cd "${bundle_dir}"
    find . -type f \
      ! -name 'files.sha256' \
      ! -name 'bundle-manifest.json' \
      ! -name 'bundle-manifest.sha256' \
      -print0 \
      | sort -z \
      | xargs -0 sha256sum
  ) >"${file_hashes}"

  python3 - \
    "${bundle_dir}" \
    "${EVIDENCE_CLASS}" \
    "${EVIDENCE_BUILD_PROFILE}" \
    "${KUBE_CONTEXT}" \
    "${KUBE_NAMESPACE}" <<'PY'
import json
import sys
from pathlib import Path

bundle = Path(sys.argv[1])
evidence_class, build_profile, context, namespace = sys.argv[2:]
window = json.loads((bundle / "metadata/window.json").read_text(encoding="utf-8"))
limitations = [
    line for line in (bundle / "limitations.txt").read_text(encoding="utf-8").splitlines() if line
]
files = []
for line in (bundle / "files.sha256").read_text(encoding="utf-8").splitlines():
    digest, path = line.split(None, 1)
    files.append({"path": path.lstrip("* ./"), "sha256": digest})

payload = {
    "schema_version": 1,
    "evidence_class": evidence_class,
    "authoritative_capacity_claim": False,
    "build_profile": build_profile,
    "kubernetes": {"context": context, "namespace": namespace},
    "window": window,
    "limitations": limitations,
    "files": files,
}
(bundle / "bundle-manifest.json").write_text(
    json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
PY
  (
    cd "${bundle_dir}"
    sha256sum bundle-manifest.json >bundle-manifest.sha256
  )
}

write_window_metadata
printf '[%s] evidence_capture phase=%s bundle=%q\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${phase}" "${bundle_dir}" >>"${commands_file}"

if [[ "${phase}" == "start" ]]; then
  {
    printf 'commit=%s\n' "$(git rev-parse HEAD 2>/dev/null || echo unavailable)"
    printf 'branch=%s\n' "$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unavailable)"
    printf 'describe=%s\n' "$(git describe --tags --always --dirty 2>/dev/null || echo unavailable)"
    if git diff --quiet --ignore-submodules -- 2>/dev/null \
      && git diff --cached --quiet --ignore-submodules -- 2>/dev/null \
      && [[ -z "$(git ls-files --others --exclude-standard 2>/dev/null)" ]]; then
      printf 'dirty=false\n'
    else
      printf 'dirty=true\n'
    fi
  } >"${metadata_dir}/git.txt"
  git status --short --branch >"${metadata_dir}/git-status.txt" 2>&1 || \
    record_limitation "git status unavailable"

  {
    printf 'build_profile=%s\n' "${EVIDENCE_BUILD_PROFILE}"
    rustc --version --verbose 2>&1 || true
    cargo --version --verbose 2>&1 || true
  } >"${metadata_dir}/rust-toolchain.txt"
  if ! command -v rustc >/dev/null 2>&1 || ! command -v cargo >/dev/null 2>&1; then
    record_limitation "Rust toolchain metadata unavailable on capture host"
  fi

  capture_command "${metadata_dir}/host-uname.txt" "host kernel metadata" uname -a
  capture_command "${metadata_dir}/host-cpu.txt" "host CPU metadata" sh -c 'command -v lscpu >/dev/null && lscpu || cat /proc/cpuinfo'
  capture_command "${metadata_dir}/host-memory.txt" "host memory metadata" sh -c 'command -v free >/dev/null && free -b || cat /proc/meminfo'
  if command -v docker >/dev/null 2>&1; then
    capture_command "${metadata_dir}/docker-version.txt" "Docker version" docker version
  else
    record_limitation "Docker version unavailable: docker CLI not installed"
  fi

  capture_input_group scenario "${EVIDENCE_SCENARIO_FILES}"
  capture_input_group descriptor "${EVIDENCE_DESCRIPTOR_FILES}"

  if command -v kubectl >/dev/null 2>&1; then
    capture_command "${metadata_dir}/kubernetes-version.txt" "Kubernetes version" \
      kubectl --context "${KUBE_CONTEXT}" version -o json
    capture_command "${metadata_dir}/kubernetes-nodes.txt" "Kubernetes node metadata" \
      kubectl --context "${KUBE_CONTEXT}" get nodes -o wide
    capture_command "${resources_dir}/pods-start.txt" "initial pod snapshot" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods -o wide
    capture_command "${resources_dir}/pod-resources-start.txt" "initial pod resource snapshot" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" top pods --containers
    capture_command "${metadata_dir}/pulse-images.txt" "Pulse image references and runtime image IDs" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods \
      -l "${EVIDENCE_PULSE_SELECTOR}" \
      -o 'custom-columns=POD:.metadata.name,IMAGE:.spec.containers[*].image,IMAGE_ID:.status.containerStatuses[*].imageID'
    capture_command "${metadata_dir}/cluster-images.txt" "cluster workload image references and runtime image IDs" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods \
      -o 'custom-columns=POD:.metadata.name,IMAGE:.spec.containers[*].image,IMAGE_ID:.status.containerStatuses[*].imageID'
    capture_redacted_deployment "${EVIDENCE_PULSE_DEPLOYMENT}" pulse
    capture_redacted_configmap "${EVIDENCE_PULSE_CONFIGMAP}" pulse
    capture_redacted_deployment "${EVIDENCE_KAFKA_DEPLOYMENT}" kafka
    capture_redacted_deployment "${EVIDENCE_REDIS_DEPLOYMENT}" redis
    capture_redacted_deployment "${EVIDENCE_TARGET_DEPLOYMENT}" target
    capture_redacted_configmap "${EVIDENCE_TARGET_CONFIGMAP}" target
    capture_command "${config_dir}/kafka-topics.txt" "Kafka topic configuration" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" exec \
      "deploy/${EVIDENCE_KAFKA_DEPLOYMENT}" -- \
      /opt/kafka/bin/kafka-topics.sh --bootstrap-server localhost:9092 --describe
    capture_command "${config_dir}/redis-safe-config.txt" "Redis persistence and eviction configuration" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" exec \
      "deploy/${EVIDENCE_REDIS_DEPLOYMENT}" -- redis-cli --raw CONFIG GET \
      appendonly save maxmemory maxmemory-policy timeout tcp-keepalive
  else
    record_limitation "Kubernetes metadata unavailable: kubectl not installed"
  fi
fi

if [[ "${phase}" == "finish" ]]; then
  if command -v kubectl >/dev/null 2>&1; then
    capture_command "${resources_dir}/pods-finish.txt" "final pod snapshot" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" get pods -o wide
    capture_command "${resources_dir}/pod-resources-finish.txt" "final pod resource snapshot" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" top pods --containers
    capture_command "${logs_dir}/pulse.log" "Pulse structured logs" \
      kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" logs \
      "deployment/${EVIDENCE_PULSE_DEPLOYMENT}" --all-containers=true --prefix=true \
      --tail="${EVIDENCE_LOG_TAIL_LINES}"
    if [[ -n "${EVIDENCE_TARGET_DEPLOYMENT}" ]]; then
      capture_command "${logs_dir}/target.log" "target-service logs" \
        kubectl --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" logs \
        "deployment/${EVIDENCE_TARGET_DEPLOYMENT}" --all-containers=true --prefix=true \
        --tail="${EVIDENCE_LOG_TAIL_LINES}"
    fi
  fi

  if [[ ! -s "${bundle_dir}/failure-timeline.jsonl" ]]; then
    printf '{"timestamp_utc":"%s","event":"no_failure_injection_recorded"}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"${bundle_dir}/failure-timeline.jsonl"
  fi
  record_limitation "external gRPC side effects cannot be atomically correlated with Kafka and Redis"
  record_limitation "unique source/result/DLQ identities are unavailable unless an external topic export is added to this bundle"
  write_window_metadata
  write_manifest
  printf 'evidence bundle finalized: %s\n' "${bundle_dir}"
fi
