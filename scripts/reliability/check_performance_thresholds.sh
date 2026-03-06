#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
export LANG=C

KUBE_CONTEXT="${KUBE_CONTEXT:-kind-account}"
KUBE_NAMESPACE="${KUBE_NAMESPACE:-pulse-dev}"
PERF_PROM_DEPLOYMENT="${PERF_PROM_DEPLOYMENT:-prometheus}"
PERF_OVERLAY="${PERF_OVERLAY:-unknown}"
PERF_WINDOW="${PERF_WINDOW:-30m}"
PERF_THRESHOLD_FILE="${PERF_THRESHOLD_FILE:-k8s/overlays/kind/performance-thresholds.csv}"
PERF_REPORT_DIR="${PERF_REPORT_DIR:-artifacts/reliability}"
PERF_HISTORY_FILE="${PERF_HISTORY_FILE:-${PERF_REPORT_DIR}/perf-history.jsonl}"
PERF_REPORT_MAX_POINTS="${PERF_REPORT_MAX_POINTS:-40}"
PERF_GRAFANA_ANNOTATE="${PERF_GRAFANA_ANNOTATE:-false}"
PERF_GRAFANA_URL="${PERF_GRAFANA_URL:-}"
PERF_GRAFANA_DASHBOARD_UID="${PERF_GRAFANA_DASHBOARD_UID:-pulse-runtime-metrics}"
PERF_GRAFANA_USER="${PERF_GRAFANA_USER:-}"
PERF_GRAFANA_PASSWORD="${PERF_GRAFANA_PASSWORD:-}"
PERF_GRAFANA_TOKEN="${PERF_GRAFANA_TOKEN:-}"
PERF_GRAFANA_TIMEOUT_SEC="${PERF_GRAFANA_TIMEOUT_SEC:-8}"
PERF_GRAFANA_VERIFY_TLS="${PERF_GRAFANA_VERIFY_TLS:-true}"

if ! command -v kubectl >/dev/null 2>&1; then
  echo "kubectl is required" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required" >&2
  exit 1
fi
if [[ ! -f "${PERF_THRESHOLD_FILE}" ]]; then
  echo "threshold file not found: ${PERF_THRESHOLD_FILE}" >&2
  exit 1
fi

mkdir -p "${PERF_REPORT_DIR}"
mkdir -p "$(dirname "${PERF_HISTORY_FILE}")"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
run_timestamp_iso="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
report_file="${PERF_REPORT_DIR}/perf-gate-${timestamp}.log"
report_json_file="${PERF_REPORT_DIR}/perf-gate-${timestamp}.json"
report_md_file="${PERF_REPORT_DIR}/perf-report-${timestamp}.md"
scenario_rows_file="$(mktemp)"

cleanup() {
  rm -f "${scenario_rows_file}"
}
trap cleanup EXIT

log() {
  local line="$1"
  printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "${line}" | tee -a "${report_file}"
}

is_number() {
  local raw="$1"
  [[ "${raw}" =~ ^[-+]?[0-9]+([.][0-9]+)?([eE][-+]?[0-9]+)?$ ]]
}

normalize_number() {
  local raw="$1"
  echo "${raw}" | tr ',' '.'
}

compare_ge() {
  local actual="$1"
  local threshold="$2"
  awk -v a="${actual}" -v b="${threshold}" 'BEGIN { exit !(a >= b) }'
}

compare_le() {
  local actual="$1"
  local threshold="$2"
  awk -v a="${actual}" -v b="${threshold}" 'BEGIN { exit !(a <= b) }'
}

is_truthy() {
  local raw
  raw="$(echo "${1:-}" | tr '[:upper:]' '[:lower:]')"
  [[ "${raw}" == "1" || "${raw}" == "true" || "${raw}" == "yes" || "${raw}" == "y" ]]
}

url_encode() {
  python3 - "$1" <<'PY'
import sys, urllib.parse
print(urllib.parse.quote(sys.argv[1], safe=""))
PY
}

extract_scalar_value() {
  python3 -c '
import json, sys

raw = sys.stdin.read()
try:
    start = raw.find("{")
    end = raw.rfind("}")
    if start == -1 or end == -1 or end <= start:
        print("nan")
        raise SystemExit(0)
    payload = json.loads(raw[start:end + 1])
    result = payload.get("data", {}).get("result", [])
    if not result:
        print("nan")
    else:
        print(result[0]["value"][1])
except Exception:
    print("nan")
'
}

prom_query_scalar() {
  local query="$1"
  local encoded
  local raw_response
  local stderr_file
  encoded="$(url_encode "${query}")"
  stderr_file="$(mktemp)"
  raw_response="$(
    kubectl --v=0 --context "${KUBE_CONTEXT}" -n "${KUBE_NAMESPACE}" exec "deploy/${PERF_PROM_DEPLOYMENT}" -- \
      sh -lc "wget -qO- 'http://127.0.0.1:9090/api/v1/query?query=${encoded}'" 2>"${stderr_file}"
  )"
  if [[ -s "${stderr_file}" ]]; then
    if grep -qvE '^[IWE][0-9]{4} ' "${stderr_file}"; then
      cat "${stderr_file}" >&2
    fi
  fi
  rm -f "${stderr_file}"
  printf '%s' "${raw_response}" | extract_scalar_value
}

compute_error_rate() {
  local success="$1"
  local failure="$2"
  awk -v s="${success}" -v f="${failure}" 'BEGIN {
    total = s + f;
    if (total <= 0) {
      print "nan";
    } else {
      printf "%.10f\n", f / total;
    }
  }'
}

write_json_report() {
  local run_status="$1"
  local run_failure_reason="$2"
  local git_sha="$3"
  local git_branch="$4"
  local git_tag="$5"

  python3 - \
    "${scenario_rows_file}" \
    "${report_json_file}" \
    "${run_timestamp_iso}" \
    "${timestamp}" \
    "${KUBE_CONTEXT}" \
    "${KUBE_NAMESPACE}" \
    "${PERF_OVERLAY}" \
    "${PERF_WINDOW}" \
    "${PERF_THRESHOLD_FILE}" \
    "${report_file}" \
    "${checked}" \
    "${failures}" \
    "${run_status}" \
    "${run_failure_reason}" \
    "${git_sha}" \
    "${git_branch}" \
    "${git_tag}" <<'PY'
import json
import sys
from pathlib import Path

(
    rows_path,
    json_path,
    ts_iso,
    run_id,
    kube_context,
    kube_namespace,
    overlay,
    perf_window,
    threshold_file,
    log_file,
    checked_raw,
    failures_raw,
    run_status,
    run_failure_reason,
    git_sha,
    git_branch,
    git_tag,
) = sys.argv[1:]

def parse_num(raw: str):
    if raw is None:
        return None
    value = raw.strip()
    if value.lower() == "nan" or value == "":
        return None
    try:
        return float(value)
    except ValueError:
        return None

scenarios = []
for line in Path(rows_path).read_text(encoding="utf-8").splitlines():
    if not line:
        continue
    cols = line.split("\t")
    if len(cols) != 12:
        continue
    (
        scenario_name,
        scenario_status,
        success_rate,
        failure_rate,
        p95_s,
        p99_s,
        error_rate,
        threshold_throughput_floor,
        threshold_p95_max_s,
        threshold_p99_max_s,
        threshold_error_rate_max,
        reasons_raw,
    ) = cols
    reasons = [] if reasons_raw in ("", "none") else reasons_raw.split("|")
    scenarios.append(
        {
            "name": scenario_name,
            "status": scenario_status,
            "reasons": reasons,
            "metrics": {
                "success_rate": parse_num(success_rate),
                "failure_rate": parse_num(failure_rate),
                "p95_s": parse_num(p95_s),
                "p99_s": parse_num(p99_s),
                "error_rate": parse_num(error_rate),
            },
            "thresholds": {
                "throughput_floor": parse_num(threshold_throughput_floor),
                "p95_max_s": parse_num(threshold_p95_max_s),
                "p99_max_s": parse_num(threshold_p99_max_s),
                "error_rate_max": parse_num(threshold_error_rate_max),
            },
        }
    )

payload = {
    "timestamp_utc": ts_iso,
    "run_id": run_id,
    "overlay": overlay,
    "kube_context": kube_context,
    "kube_namespace": kube_namespace,
    "perf_window": perf_window,
    "threshold_file": threshold_file,
    "summary": {
        "status": run_status,
        "checked": int(checked_raw),
        "failures": int(failures_raw),
        "failure_reason": None if run_failure_reason in ("", "none") else run_failure_reason,
    },
    "git": {
        "sha": git_sha,
        "branch": git_branch,
        "tag": None if git_tag in ("", "none") else git_tag,
    },
    "artifacts": {
        "log_file": log_file,
        "json_file": json_path,
    },
    "scenarios": scenarios,
}

Path(json_path).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
}

append_history_record() {
  python3 - \
    "${report_json_file}" \
    "${PERF_HISTORY_FILE}" <<'PY'
import json
import sys
from pathlib import Path

source_path = Path(sys.argv[1])
history_path = Path(sys.argv[2])

record = json.loads(source_path.read_text(encoding="utf-8"))

with history_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(record, separators=(",", ":")) + "\n")
PY
}

generate_markdown_report() {
  python3 scripts/reliability/generate_perf_report.py \
    --history-file "${PERF_HISTORY_FILE}" \
    --run-json "${report_json_file}" \
    --output-file "${report_md_file}" \
    --max-points "${PERF_REPORT_MAX_POINTS}"
}

publish_grafana_annotation() {
  if ! is_truthy "${PERF_GRAFANA_ANNOTATE}"; then
    return 0
  fi
  if [[ -z "${PERF_GRAFANA_URL}" ]]; then
    log "grafana annotation skipped: PERF_GRAFANA_URL is empty"
    return 0
  fi

  local annotation_output
  if ! annotation_output="$(
    python3 - \
      "${report_json_file}" \
      "${PERF_GRAFANA_URL}" \
      "${PERF_GRAFANA_DASHBOARD_UID}" \
      "${PERF_GRAFANA_USER}" \
      "${PERF_GRAFANA_PASSWORD}" \
      "${PERF_GRAFANA_TOKEN}" \
      "${PERF_GRAFANA_TIMEOUT_SEC}" \
      "${PERF_GRAFANA_VERIFY_TLS}" <<'PY'
import base64
import datetime as dt
import json
import ssl
import sys
import urllib.error
import urllib.request

(
    report_json_path,
    grafana_url,
    dashboard_uid,
    grafana_user,
    grafana_password,
    grafana_token,
    timeout_sec_raw,
    verify_tls_raw,
) = sys.argv[1:]

def parse_timeout(raw: str) -> float:
    try:
        return max(float(raw), 1.0)
    except ValueError:
        return 8.0

def is_truthy(raw: str) -> bool:
    return raw.strip().lower() in {"1", "true", "yes", "y"}

report = json.loads(open(report_json_path, encoding="utf-8").read())
timestamp_utc = report.get("timestamp_utc")
if not timestamp_utc:
    raise SystemExit("missing timestamp_utc in report json")

event_time_ms = int(dt.datetime.strptime(timestamp_utc, "%Y-%m-%dT%H:%M:%SZ").timestamp() * 1000)
overlay = report.get("overlay", "unknown")
summary = report.get("summary", {})
git_info = report.get("git", {})
git_sha = git_info.get("sha") or "unknown"
git_tag = git_info.get("tag") or "none"
status = summary.get("status", "UNKNOWN")
checked = summary.get("checked", 0)
failures = summary.get("failures", 0)

scenario_states = []
for scenario in report.get("scenarios", []):
    name = scenario.get("name", "unknown")
    scenario_status = scenario.get("status", "UNKNOWN")
    scenario_states.append(f"{name}={scenario_status}")

scenario_summary = ", ".join(scenario_states[:8])
if len(scenario_states) > 8:
    scenario_summary = f"{scenario_summary}, +{len(scenario_states) - 8} more"

text_parts = [
    f"perf_gate={status}",
    f"overlay={overlay}",
    f"checked={checked}",
    f"failures={failures}",
    f"git_sha={git_sha}",
]
if git_tag != "none":
    text_parts.append(f"git_tag={git_tag}")
if scenario_summary:
    text_parts.append(f"scenarios: {scenario_summary}")

payload = {
    "dashboardUID": dashboard_uid,
    "time": event_time_ms,
    "tags": [
        "pulse",
        "perf-gate",
        f"overlay:{overlay}",
        f"status:{status}",
        f"git_sha:{git_sha}",
        f"git_tag:{git_tag}",
    ],
    "text": " | ".join(text_parts),
}

url = grafana_url.rstrip("/") + "/api/annotations"
headers = {"Content-Type": "application/json"}
if grafana_token:
    headers["Authorization"] = f"Bearer {grafana_token}"
elif grafana_user or grafana_password:
    raw = f"{grafana_user}:{grafana_password}".encode("utf-8")
    headers["Authorization"] = "Basic " + base64.b64encode(raw).decode("ascii")

request = urllib.request.Request(
    url,
    data=json.dumps(payload).encode("utf-8"),
    headers=headers,
    method="POST",
)

context = None
if not is_truthy(verify_tls_raw):
    context = ssl._create_unverified_context()

try:
    with urllib.request.urlopen(request, timeout=parse_timeout(timeout_sec_raw), context=context) as response:
        body = response.read().decode("utf-8", "replace")
except urllib.error.HTTPError as err:
    detail = err.read().decode("utf-8", "replace")
    raise SystemExit(f"grafana annotation request failed: HTTP {err.code}: {detail.strip()}")
except urllib.error.URLError as err:
    raise SystemExit(f"grafana annotation request failed: {err.reason}")

try:
    parsed = json.loads(body)
except json.JSONDecodeError:
    parsed = {}
annotation_id = parsed.get("id", "unknown")
print(
    f"grafana annotation created id={annotation_id} dashboard_uid={dashboard_uid} "
    f"status={status} git_sha={git_sha} git_tag={git_tag}"
)
PY
  2>&1)"; then
    log "grafana annotation failed (continuing): ${annotation_output}"
    return 0
  fi

  if [[ -n "${annotation_output}" ]]; then
    log "${annotation_output}"
  fi
}

log "starting performance threshold checks"
log "context=${KUBE_CONTEXT} namespace=${KUBE_NAMESPACE} window=${PERF_WINDOW} threshold_file=${PERF_THRESHOLD_FILE}"

failures=0
checked=0
run_status="PASS"
run_failure_reason="none"
git_sha="$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")"
git_branch="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "unknown")"
git_tag="$(git describe --tags --exact-match 2>/dev/null || echo "none")"

while IFS=',' read -r scenario throughput_floor p95_max p99_max error_rate_max; do
  if [[ -z "${scenario}" ]]; then
    continue
  fi
  if [[ "${scenario}" =~ ^[[:space:]]*# ]]; then
    continue
  fi
  if [[ "${scenario}" == "scenario" ]]; then
    continue
  fi

  checked=$((checked + 1))
  scenario_trimmed="$(echo "${scenario}" | xargs)"
  throughput_floor="$(echo "${throughput_floor}" | xargs)"
  p95_max="$(echo "${p95_max}" | xargs)"
  p99_max="$(echo "${p99_max}" | xargs)"
  error_rate_max="$(echo "${error_rate_max}" | xargs)"

  success_rate="$(prom_query_scalar "sum(rate(pulse_scenario_executions_total{scenario=\"${scenario_trimmed}\",status=\"success\"}[${PERF_WINDOW}]))")"
  failure_rate="$(prom_query_scalar "sum(rate(pulse_scenario_executions_total{scenario=\"${scenario_trimmed}\",status=\"failure\"}[${PERF_WINDOW}]))")"
  p95_value="$(prom_query_scalar "histogram_quantile(0.95, sum by (le) (rate(pulse_scenario_duration_seconds_bucket{scenario=\"${scenario_trimmed}\",status=\"success\"}[${PERF_WINDOW}])))")"
  p99_value="$(prom_query_scalar "histogram_quantile(0.99, sum by (le) (rate(pulse_scenario_duration_seconds_bucket{scenario=\"${scenario_trimmed}\",status=\"success\"}[${PERF_WINDOW}])))")"
  success_rate="$(normalize_number "${success_rate}")"
  failure_rate="$(normalize_number "${failure_rate}")"
  p95_value="$(normalize_number "${p95_value}")"
  p99_value="$(normalize_number "${p99_value}")"

  if ! is_number "${success_rate}"; then
    success_rate="nan"
  fi
  if ! is_number "${failure_rate}"; then
    failure_rate="0"
  fi

  error_rate="$(compute_error_rate "${success_rate}" "${failure_rate}")"
  error_rate="$(normalize_number "${error_rate}")"

  line_status="PASS"
  line_reasons=()

  if ! is_number "${success_rate}" || ! is_number "${p95_value}" || ! is_number "${p99_value}" || ! is_number "${error_rate}"; then
    line_status="FAIL"
    line_reasons+=("missing_or_invalid_metrics")
  else
    if ! compare_ge "${success_rate}" "${throughput_floor}"; then
      line_status="FAIL"
      line_reasons+=("throughput")
    fi
    if ! compare_le "${p95_value}" "${p95_max}"; then
      line_status="FAIL"
      line_reasons+=("p95")
    fi
    if ! compare_le "${p99_value}" "${p99_max}"; then
      line_status="FAIL"
      line_reasons+=("p99")
    fi
    if ! compare_le "${error_rate}" "${error_rate_max}"; then
      line_status="FAIL"
      line_reasons+=("error_rate")
    fi
  fi

  line_reasons_joined="$(IFS='|'; echo "${line_reasons[*]:-none}")"
  log "scenario=${scenario_trimmed} status=${line_status} success_rate=${success_rate} floor=${throughput_floor} p95_s=${p95_value} p95_max_s=${p95_max} p99_s=${p99_value} p99_max_s=${p99_max} error_rate=${error_rate} error_rate_max=${error_rate_max} reasons=${line_reasons_joined}"
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "${scenario_trimmed}" \
    "${line_status}" \
    "${success_rate}" \
    "${failure_rate}" \
    "${p95_value}" \
    "${p99_value}" \
    "${error_rate}" \
    "${throughput_floor}" \
    "${p95_max}" \
    "${p99_max}" \
    "${error_rate_max}" \
    "${line_reasons_joined}" >> "${scenario_rows_file}"

  if [[ "${line_status}" == "FAIL" ]]; then
    failures=$((failures + 1))
  fi
done < "${PERF_THRESHOLD_FILE}"

if (( checked == 0 )); then
  run_status="FAIL"
  run_failure_reason="no_thresholds_loaded"
  failures=$((failures + 1))
  log "no thresholds loaded from ${PERF_THRESHOLD_FILE}"
fi

if (( failures > 0 )); then
  run_status="FAIL"
fi

log "performance threshold checks completed checked=${checked} failures=${failures} report_file=${report_file}"
write_json_report "${run_status}" "${run_failure_reason}" "${git_sha}" "${git_branch}" "${git_tag}"
log "json report saved to ${report_json_file}"
append_history_record
log "history updated in ${PERF_HISTORY_FILE}"
generate_markdown_report
log "markdown report saved to ${report_md_file}"
publish_grafana_annotation

if [[ "${run_status}" != "PASS" ]]; then
  exit 1
fi
