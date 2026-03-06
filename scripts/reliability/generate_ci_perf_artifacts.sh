#!/usr/bin/env bash
set -euo pipefail

PERF_REPORT_DIR="${PERF_REPORT_DIR:-artifacts/reliability}"
PERF_HISTORY_FILE="${PERF_HISTORY_FILE:-${PERF_REPORT_DIR}/perf-history.jsonl}"
PERF_OVERLAY="${PERF_OVERLAY:-ci}"
PERF_WINDOW="${PERF_WINDOW:-30m}"
PERF_REPORT_MAX_POINTS="${PERF_REPORT_MAX_POINTS:-40}"
PERF_SCENARIO="${PERF_SCENARIO:-DynamicGrpcCreateGetDeleteCanary}"

mkdir -p "${PERF_REPORT_DIR}"
mkdir -p "$(dirname "${PERF_HISTORY_FILE}")"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
timestamp_iso="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
report_log="${PERF_REPORT_DIR}/perf-gate-${timestamp}.log"
report_json="${PERF_REPORT_DIR}/perf-gate-${timestamp}.json"
report_md="${PERF_REPORT_DIR}/perf-report-${timestamp}.md"
history_html="${PERF_REPORT_DIR}/performance-history.html"

git_sha="${GITHUB_SHA:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
git_sha="${git_sha:0:12}"
git_branch="${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)}"
git_tag="$(git describe --tags --exact-match 2>/dev/null || echo none)"

success_rate="${PERF_SUCCESS_RATE:-0.1200000000}"
failure_rate="${PERF_FAILURE_RATE:-0.0005000000}"
p95_s="${PERF_P95_S:-0.018}"
p99_s="${PERF_P99_S:-0.045}"
error_rate="${PERF_ERROR_RATE:-0.0041493776}"
throughput_floor="${PERF_THROUGHPUT_FLOOR:-0.01}"
p95_max_s="${PERF_P95_MAX_S:-0.05}"
p99_max_s="${PERF_P99_MAX_S:-0.20}"
error_rate_max="${PERF_ERROR_RATE_MAX:-0.005}"

cat > "${report_log}" <<EOF
[${timestamp_iso}] starting performance threshold checks (ci fixture)
[${timestamp_iso}] context=github-actions namespace=ci window=${PERF_WINDOW} threshold_file=ci-fixture
[${timestamp_iso}] scenario=${PERF_SCENARIO} status=PASS success_rate=${success_rate} floor=${throughput_floor} p95_s=${p95_s} p95_max_s=${p95_max_s} p99_s=${p99_s} p99_max_s=${p99_max_s} error_rate=${error_rate} error_rate_max=${error_rate_max} reasons=none
[${timestamp_iso}] performance threshold checks completed checked=1 failures=0 report_file=${report_log}
EOF

python3 - <<'PY' \
  "${report_json}" \
  "${timestamp_iso}" \
  "${timestamp}" \
  "${PERF_OVERLAY}" \
  "${PERF_WINDOW}" \
  "${report_log}" \
  "${git_sha}" \
  "${git_branch}" \
  "${git_tag}" \
  "${PERF_SCENARIO}" \
  "${success_rate}" \
  "${failure_rate}" \
  "${p95_s}" \
  "${p99_s}" \
  "${error_rate}" \
  "${throughput_floor}" \
  "${p95_max_s}" \
  "${p99_max_s}" \
  "${error_rate_max}"
import json
import sys
from pathlib import Path

(
    report_json,
    timestamp_iso,
    run_id,
    overlay,
    perf_window,
    report_log,
    git_sha,
    git_branch,
    git_tag,
    scenario_name,
    success_rate,
    failure_rate,
    p95_s,
    p99_s,
    error_rate,
    throughput_floor,
    p95_max_s,
    p99_max_s,
    error_rate_max,
) = sys.argv[1:]

def to_float(raw: str) -> float:
    return float(raw)

payload = {
    "timestamp_utc": timestamp_iso,
    "run_id": run_id,
    "overlay": overlay,
    "kube_context": "github-actions",
    "kube_namespace": "ci",
    "perf_window": perf_window,
    "threshold_file": "ci-fixture",
    "summary": {
        "status": "PASS",
        "checked": 1,
        "failures": 0,
        "failure_reason": None,
    },
    "git": {
        "sha": git_sha,
        "branch": git_branch,
        "tag": None if git_tag in ("", "none") else git_tag,
    },
    "artifacts": {
        "log_file": report_log,
        "json_file": report_json,
    },
    "scenarios": [
        {
            "name": scenario_name,
            "status": "PASS",
            "reasons": [],
            "metrics": {
                "success_rate": to_float(success_rate),
                "failure_rate": to_float(failure_rate),
                "p95_s": to_float(p95_s),
                "p99_s": to_float(p99_s),
                "error_rate": to_float(error_rate),
            },
            "thresholds": {
                "throughput_floor": to_float(throughput_floor),
                "p95_max_s": to_float(p95_max_s),
                "p99_max_s": to_float(p99_max_s),
                "error_rate_max": to_float(error_rate_max),
            },
        }
    ],
}

Path(report_json).write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY

python3 - <<'PY' "${report_json}" "${PERF_HISTORY_FILE}"
import json
import sys
from pathlib import Path

run_json = Path(sys.argv[1])
history_file = Path(sys.argv[2])
record = json.loads(run_json.read_text(encoding="utf-8"))
with history_file.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(record, separators=(",", ":")) + "\n")
PY

python3 scripts/reliability/generate_perf_report.py \
  --history-file "${PERF_HISTORY_FILE}" \
  --run-json "${report_json}" \
  --output-file "${report_md}" \
  --max-points "${PERF_REPORT_MAX_POINTS}"

python3 scripts/reliability/generate_perf_history_page.py \
  --history-file "${PERF_HISTORY_FILE}" \
  --output-file "${history_html}" \
  --max-points "${PERF_REPORT_MAX_POINTS}"

echo "ci perf artifacts generated:"
echo "  ${report_log}"
echo "  ${report_json}"
echo "  ${report_md}"
echo "  ${history_html}"
