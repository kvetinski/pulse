#!/usr/bin/env python3
"""Generate a visual markdown performance report from perf history JSONL."""

from __future__ import annotations

import argparse
import json
import math
import re
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate markdown + SVG trend charts from perf-history.jsonl"
    )
    parser.add_argument("--history-file", required=True, help="Path to perf-history.jsonl")
    parser.add_argument("--run-json", required=True, help="Path to current run json file")
    parser.add_argument("--output-file", required=True, help="Markdown output path")
    parser.add_argument(
        "--max-points",
        type=int,
        default=40,
        help="Maximum history points per scenario to render",
    )
    return parser.parse_args()


def to_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def load_history(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    if not path.exists():
        return records
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return records


def slugify(value: str) -> str:
    slug = re.sub(r"[^a-zA-Z0-9]+", "-", value.strip().lower()).strip("-")
    return slug or "scenario"


def scenario_entry(record: dict[str, Any], name: str) -> dict[str, Any] | None:
    for item in record.get("scenarios", []):
        if item.get("name") == name:
            return item
    return None


def collect_points(
    records: list[dict[str, Any]], scenario_name: str, max_points: int
) -> list[dict[str, Any]]:
    points: list[dict[str, Any]] = []
    for record in records:
        entry = scenario_entry(record, scenario_name)
        if not entry:
            continue
        metrics = entry.get("metrics", {})
        points.append(
            {
                "run_id": record.get("run_id", ""),
                "timestamp_utc": record.get("timestamp_utc", ""),
                "status": entry.get("status", record.get("summary", {}).get("status", "FAIL")),
                "success_rate": to_float(metrics.get("success_rate")),
                "p95_s": to_float(metrics.get("p95_s")),
                "p99_s": to_float(metrics.get("p99_s")),
                "error_rate": to_float(metrics.get("error_rate")),
            }
        )
    points.sort(key=lambda x: (x["timestamp_utc"], x["run_id"]))
    if max_points > 0:
        points = points[-max_points:]
    return points


def fmt_num(value: float | None, precision: int = 6) -> str:
    if value is None or math.isnan(value):
        return "n/a"
    return f"{value:.{precision}f}"


def fmt_delta(current: float | None, previous: float | None) -> str:
    if current is None or previous is None:
        return "n/a"
    delta = current - previous
    if abs(delta) < 1e-12:
        return "0.000000"
    sign = "+" if delta > 0 else ""
    return f"{sign}{delta:.6f}"


def write_line_chart(
    path: Path,
    title: str,
    points: list[dict[str, Any]],
    series: list[tuple[str, str, str]],
    thresholds: list[tuple[str, float | None, str]],
    y_label: str,
) -> None:
    width = 960
    height = 320
    left = 70
    right = 30
    top = 40
    bottom = 60
    chart_w = width - left - right
    chart_h = height - top - bottom

    values: list[float] = []
    for _, key, _ in series:
        for point in points:
            val = to_float(point.get(key))
            if val is not None:
                values.append(val)
    for _, threshold, _ in thresholds:
        if threshold is not None:
            values.append(threshold)
    if not values:
        values = [0.0, 1.0]

    y_min = 0.0
    y_max = max(values)
    if y_max <= y_min:
        y_max = y_min + 1.0
    y_pad = max((y_max - y_min) * 0.1, 1e-6)
    y_max += y_pad

    n = max(len(points), 1)

    def x_at(index: int) -> float:
        if n == 1:
            return left + chart_w / 2
        return left + (index * chart_w / (n - 1))

    def y_at(value: float) -> float:
        ratio = (value - y_min) / (y_max - y_min)
        ratio = max(0.0, min(1.0, ratio))
        return top + chart_h - ratio * chart_h

    lines: list[str] = []
    lines.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}">'
    )
    lines.append('<rect x="0" y="0" width="100%" height="100%" fill="white" />')
    lines.append(
        f'<text x="{left}" y="24" font-family="monospace" font-size="16" fill="#111">{title}</text>'
    )

    grid_steps = 5
    for idx in range(grid_steps + 1):
        y_val = y_min + (y_max - y_min) * idx / grid_steps
        y_pos = y_at(y_val)
        lines.append(
            f'<line x1="{left}" y1="{y_pos:.2f}" x2="{left + chart_w}" y2="{y_pos:.2f}" '
            'stroke="#e5e7eb" stroke-width="1" />'
        )
        lines.append(
            f'<text x="{left - 10}" y="{y_pos + 4:.2f}" text-anchor="end" '
            f'font-family="monospace" font-size="10" fill="#6b7280">{y_val:.4f}</text>'
        )

    lines.append(
        f'<line x1="{left}" y1="{top}" x2="{left}" y2="{top + chart_h}" stroke="#111" stroke-width="1.5" />'
    )
    lines.append(
        f'<line x1="{left}" y1="{top + chart_h}" x2="{left + chart_w}" y2="{top + chart_h}" stroke="#111" stroke-width="1.5" />'
    )

    for label, threshold, color in thresholds:
        if threshold is None:
            continue
        y_pos = y_at(threshold)
        lines.append(
            f'<line x1="{left}" y1="{y_pos:.2f}" x2="{left + chart_w}" y2="{y_pos:.2f}" '
            f'stroke="{color}" stroke-width="1.2" stroke-dasharray="5,4" />'
        )
        lines.append(
            f'<text x="{left + chart_w - 2}" y="{y_pos - 4:.2f}" text-anchor="end" '
            f'font-family="monospace" font-size="10" fill="{color}">{label}: {threshold:.6f}</text>'
        )

    legend_x = left
    legend_y = height - 18
    for label, key, color in series:
        coords: list[str] = []
        for i, point in enumerate(points):
            value = to_float(point.get(key))
            if value is None:
                continue
            coords.append(f"{x_at(i):.2f},{y_at(value):.2f}")
        if len(coords) >= 2:
            lines.append(
                f'<polyline fill="none" stroke="{color}" stroke-width="2" points="{" ".join(coords)}" />'
            )
        elif len(coords) == 1:
            x_pos, y_pos = coords[0].split(",")
            lines.append(f'<circle cx="{x_pos}" cy="{y_pos}" r="2.8" fill="{color}" />')

        lines.append(
            f'<line x1="{legend_x}" y1="{legend_y - 4}" x2="{legend_x + 16}" y2="{legend_y - 4}" '
            f'stroke="{color}" stroke-width="2" />'
        )
        lines.append(
            f'<text x="{legend_x + 20}" y="{legend_y}" font-family="monospace" font-size="11" fill="#111">{label}</text>'
        )
        legend_x += 180

    if points:
        first = points[0]["run_id"]
        last = points[-1]["run_id"]
        lines.append(
            f'<text x="{left}" y="{top + chart_h + 24}" font-family="monospace" font-size="10" fill="#6b7280">{first}</text>'
        )
        lines.append(
            f'<text x="{left + chart_w}" y="{top + chart_h + 24}" text-anchor="end" '
            f'font-family="monospace" font-size="10" fill="#6b7280">{last}</text>'
        )

    lines.append(
        f'<text x="{left - 50}" y="{top + chart_h / 2:.2f}" transform="rotate(-90 {left - 50},{top + chart_h / 2:.2f})" '
        f'font-family="monospace" font-size="11" fill="#374151">{y_label}</text>'
    )
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_status_timeline(path: Path, title: str, points: list[dict[str, Any]]) -> None:
    width = 960
    height = 180
    left = 70
    right = 30
    top = 35
    bottom = 45
    chart_w = width - left - right
    chart_h = height - top - bottom
    n = max(len(points), 1)
    bar_w = chart_w / n

    lines: list[str] = []
    lines.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}">'
    )
    lines.append('<rect x="0" y="0" width="100%" height="100%" fill="white" />')
    lines.append(
        f'<text x="{left}" y="22" font-family="monospace" font-size="16" fill="#111">{title}</text>'
    )
    lines.append(
        f'<rect x="{left}" y="{top}" width="{chart_w}" height="{chart_h}" fill="#f9fafb" stroke="#d1d5db" />'
    )

    for idx, point in enumerate(points):
        status = str(point.get("status", "FAIL")).upper()
        color = "#16a34a" if status == "PASS" else "#dc2626"
        x_pos = left + idx * bar_w
        lines.append(
            f'<rect x="{x_pos + 1:.2f}" y="{top + 1}" width="{max(bar_w - 2, 1):.2f}" '
            f'height="{chart_h - 2}" fill="{color}" opacity="0.85" />'
        )

    if points:
        lines.append(
            f'<text x="{left}" y="{top + chart_h + 20}" font-family="monospace" font-size="10" fill="#6b7280">{points[0]["run_id"]}</text>'
        )
        lines.append(
            f'<text x="{left + chart_w}" y="{top + chart_h + 20}" text-anchor="end" font-family="monospace" font-size="10" fill="#6b7280">{points[-1]["run_id"]}</text>'
        )

    legend_y = height - 12
    lines.append(
        f'<rect x="{left}" y="{legend_y - 8}" width="12" height="8" fill="#16a34a" opacity="0.85" />'
    )
    lines.append(
        f'<text x="{left + 18}" y="{legend_y}" font-family="monospace" font-size="11" fill="#111">PASS</text>'
    )
    lines.append(
        f'<rect x="{left + 90}" y="{legend_y - 8}" width="12" height="8" fill="#dc2626" opacity="0.85" />'
    )
    lines.append(
        f'<text x="{left + 108}" y="{legend_y}" font-family="monospace" font-size="11" fill="#111">FAIL</text>'
    )
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def scenario_thresholds(latest: dict[str, Any], scenario_name: str) -> dict[str, float | None]:
    entry = scenario_entry(latest, scenario_name) or {}
    thresholds = entry.get("thresholds", {})
    return {
        "throughput_floor": to_float(thresholds.get("throughput_floor")),
        "p95_max_s": to_float(thresholds.get("p95_max_s")),
        "p99_max_s": to_float(thresholds.get("p99_max_s")),
        "error_rate_max": to_float(thresholds.get("error_rate_max")),
    }


def metric_from_point(point: dict[str, Any], key: str) -> float | None:
    return to_float(point.get(key))


def build_report(
    latest: dict[str, Any], history: list[dict[str, Any]], output_file: Path, max_points: int
) -> None:
    output_file.parent.mkdir(parents=True, exist_ok=True)
    run_id = latest.get("run_id", "unknown")
    overlay = latest.get("overlay", "unknown")

    md: list[str] = []
    md.append(f"# Performance Report {run_id}")
    md.append("")
    md.append("## Run Metadata")
    md.append("")
    md.append(f"- `timestamp_utc`: `{latest.get('timestamp_utc', 'unknown')}`")
    md.append(f"- `overlay`: `{overlay}`")
    md.append(f"- `kube_context`: `{latest.get('kube_context', 'unknown')}`")
    md.append(f"- `kube_namespace`: `{latest.get('kube_namespace', 'unknown')}`")
    md.append(f"- `perf_window`: `{latest.get('perf_window', 'unknown')}`")
    md.append(f"- `status`: `{latest.get('summary', {}).get('status', 'unknown')}`")
    md.append(
        f"- `git_sha`: `{latest.get('git', {}).get('sha', 'unknown')}`"
    )
    md.append(
        f"- `git_branch`: `{latest.get('git', {}).get('branch', 'unknown')}`"
    )
    md.append(
        f"- `git_tag`: `{latest.get('git', {}).get('tag') or 'none'}`"
    )
    md.append("")

    scenarios = latest.get("scenarios", [])
    if not scenarios:
        md.append("No scenarios in current run.")
        output_file.write_text("\n".join(md) + "\n", encoding="utf-8")
        return

    for current in scenarios:
        scenario_name = str(current.get("name", "unknown"))
        slug = slugify(scenario_name)
        points = collect_points(history, scenario_name, max_points)
        thresholds = scenario_thresholds(latest, scenario_name)
        current_point = points[-1] if points else {}
        previous_point = points[-2] if len(points) > 1 else {}
        reasons = current.get("reasons", [])
        reason_text = ", ".join(reasons) if reasons else "none"

        throughput_svg = output_file.parent / f"perf-report-{run_id}-{slug}-throughput.svg"
        latency_svg = output_file.parent / f"perf-report-{run_id}-{slug}-latency.svg"
        error_svg = output_file.parent / f"perf-report-{run_id}-{slug}-error-rate.svg"
        status_svg = output_file.parent / f"perf-report-{run_id}-{slug}-status.svg"

        write_line_chart(
            throughput_svg,
            f"{scenario_name} - Throughput Trend",
            points,
            [("success_rate", "success_rate", "#2563eb")],
            [("floor", thresholds["throughput_floor"], "#dc2626")],
            "scenario/s",
        )
        write_line_chart(
            latency_svg,
            f"{scenario_name} - Latency Trend",
            points,
            [("p95_s", "p95_s", "#2563eb"), ("p99_s", "p99_s", "#d97706")],
            [
                ("p95_max", thresholds["p95_max_s"], "#1d4ed8"),
                ("p99_max", thresholds["p99_max_s"], "#b45309"),
            ],
            "seconds",
        )
        write_line_chart(
            error_svg,
            f"{scenario_name} - Error Rate Trend",
            points,
            [("error_rate", "error_rate", "#7c3aed")],
            [("max", thresholds["error_rate_max"], "#dc2626")],
            "ratio",
        )
        write_status_timeline(status_svg, f"{scenario_name} - Pass/Fail Timeline", points)

        md.append(f"## Scenario: `{scenario_name}`")
        md.append("")
        md.append(f"- current status: `{current.get('status', 'unknown')}`")
        md.append(f"- reasons: `{reason_text}`")
        md.append(f"- points rendered: `{len(points)}`")
        md.append("")
        md.append("### Delta vs Previous Run")
        md.append("")
        md.append("| Metric | Current | Previous | Delta | Threshold |")
        md.append("|---|---:|---:|---:|---:|")
        metric_rows = [
            ("success_rate", "throughput_floor"),
            ("p95_s", "p95_max_s"),
            ("p99_s", "p99_max_s"),
            ("error_rate", "error_rate_max"),
        ]
        for metric, threshold_key in metric_rows:
            current_val = metric_from_point(current_point, metric)
            previous_val = metric_from_point(previous_point, metric)
            threshold_val = thresholds.get(threshold_key)
            md.append(
                f"| `{metric}` | {fmt_num(current_val)} | {fmt_num(previous_val)} | "
                f"{fmt_delta(current_val, previous_val)} | {fmt_num(threshold_val)} |"
            )
        md.append("")
        md.append("### Trend Charts")
        md.append("")
        md.append(f"![Throughput Trend]({throughput_svg.name})")
        md.append("")
        md.append(f"![Latency Trend]({latency_svg.name})")
        md.append("")
        md.append(f"![Error Rate Trend]({error_svg.name})")
        md.append("")
        md.append(f"![Pass Fail Timeline]({status_svg.name})")
        md.append("")

    output_file.write_text("\n".join(md) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    history_file = Path(args.history_file)
    run_json_file = Path(args.run_json)
    output_file = Path(args.output_file)

    latest = load_json(run_json_file)
    history = load_history(history_file)
    build_report(latest, history, output_file, args.max_points)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
