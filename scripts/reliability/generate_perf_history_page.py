#!/usr/bin/env python3
"""Generate a static performance history page and trend charts from perf-history.jsonl."""

from __future__ import annotations

import argparse
import json
import math
import re
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate performance history HTML + SVG charts from perf-history.jsonl"
    )
    parser.add_argument("--history-file", required=True, help="Path to perf-history.jsonl")
    parser.add_argument("--output-file", required=True, help="HTML output path")
    parser.add_argument(
        "--max-points",
        type=int,
        default=60,
        help="Maximum history points per scenario in charts",
    )
    return parser.parse_args()


def load_history(path: Path) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    if not path.exists():
        return records
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            records.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    records.sort(key=lambda x: (str(x.get("timestamp_utc", "")), str(x.get("run_id", ""))))
    return records


def to_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def slugify(value: str) -> str:
    slug = re.sub(r"[^a-zA-Z0-9]+", "-", value.strip().lower()).strip("-")
    return slug or "scenario"


def scenario_names(records: list[dict[str, Any]]) -> list[str]:
    names: set[str] = set()
    for record in records:
        for item in record.get("scenarios", []):
            name = item.get("name")
            if isinstance(name, str) and name:
                names.add(name)
    return sorted(names)


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
        thresholds = entry.get("thresholds", {})
        points.append(
            {
                "run_id": str(record.get("run_id", "")),
                "timestamp_utc": str(record.get("timestamp_utc", "")),
                "overlay": str(record.get("overlay", "")),
                "git_sha": str(record.get("git", {}).get("sha", "unknown")),
                "status": str(entry.get("status", record.get("summary", {}).get("status", "FAIL"))),
                "success_rate": to_float(metrics.get("success_rate")),
                "p95_s": to_float(metrics.get("p95_s")),
                "p99_s": to_float(metrics.get("p99_s")),
                "error_rate": to_float(metrics.get("error_rate")),
                "throughput_floor": to_float(thresholds.get("throughput_floor")),
                "p95_max_s": to_float(thresholds.get("p95_max_s")),
                "p99_max_s": to_float(thresholds.get("p99_max_s")),
                "error_rate_max": to_float(thresholds.get("error_rate_max")),
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


def latest_threshold(points: list[dict[str, Any]], key: str) -> float | None:
    for point in reversed(points):
        threshold = to_float(point.get(key))
        if threshold is not None:
            return threshold
    return None


def write_line_chart(
    path: Path,
    title: str,
    points: list[dict[str, Any]],
    series: list[tuple[str, str, str]],
    thresholds: list[tuple[str, float | None, str]],
    y_label: str,
) -> None:
    width = 1000
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
            value = to_float(point.get(key))
            if value is not None:
                values.append(value)
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
        for idx, point in enumerate(points):
            value = to_float(point.get(key))
            if value is None:
                continue
            coords.append(f"{x_at(idx):.2f},{y_at(value):.2f}")
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
        lines.append(
            f'<text x="{left}" y="{top + chart_h + 24}" font-family="monospace" font-size="10" fill="#6b7280">{points[0]["run_id"]}</text>'
        )
        lines.append(
            f'<text x="{left + chart_w}" y="{top + chart_h + 24}" text-anchor="end" '
            f'font-family="monospace" font-size="10" fill="#6b7280">{points[-1]["run_id"]}</text>'
        )

    lines.append(
        f'<text x="{left - 50}" y="{top + chart_h / 2:.2f}" transform="rotate(-90 {left - 50},{top + chart_h / 2:.2f})" '
        f'font-family="monospace" font-size="11" fill="#374151">{y_label}</text>'
    )
    lines.append("</svg>")
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_status_timeline(path: Path, title: str, points: list[dict[str, Any]]) -> None:
    width = 1000
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
            f'<text x="{left + chart_w}" y="{top + chart_h + 20}" text-anchor="end" '
            f'font-family="monospace" font-size="10" fill="#6b7280">{points[-1]["run_id"]}</text>'
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


def build_history_page(
    records: list[dict[str, Any]], output_file: Path, max_points: int
) -> None:
    output_file.parent.mkdir(parents=True, exist_ok=True)
    all_scenarios = scenario_names(records)
    latest = records[-1] if records else {}
    run_count = len(records)
    latest_run_id = str(latest.get("run_id", "n/a"))
    latest_timestamp = str(latest.get("timestamp_utc", "n/a"))
    latest_overlay = str(latest.get("overlay", "n/a"))

    lines: list[str] = []
    lines.append("<!doctype html>")
    lines.append('<html lang="en">')
    lines.append("<head>")
    lines.append('  <meta charset="utf-8">')
    lines.append('  <meta name="viewport" content="width=device-width, initial-scale=1">')
    lines.append("  <title>Pulse Performance History</title>")
    lines.append("  <style>")
    lines.append("    body { font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; margin: 24px; color: #111827; background: #f8fafc; }")
    lines.append("    h1, h2, h3 { margin: 0 0 10px 0; }")
    lines.append("    .muted { color: #4b5563; }")
    lines.append("    .meta { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 10px; margin: 16px 0 24px; }")
    lines.append("    .card { background: #ffffff; border: 1px solid #d1d5db; border-radius: 10px; padding: 12px 14px; }")
    lines.append("    .scenario { margin: 28px 0; }")
    lines.append("    .charts img { width: 100%; max-width: 1000px; border: 1px solid #e5e7eb; border-radius: 8px; background: #fff; margin: 8px 0 14px; }")
    lines.append("    table { border-collapse: collapse; width: 100%; max-width: 1000px; background: #fff; }")
    lines.append("    th, td { border: 1px solid #e5e7eb; padding: 8px 10px; text-align: left; font-size: 13px; }")
    lines.append("    th { background: #f3f4f6; }")
    lines.append("    .pass { color: #166534; font-weight: 700; }")
    lines.append("    .fail { color: #991b1b; font-weight: 700; }")
    lines.append("    a { color: #1d4ed8; }")
    lines.append("  </style>")
    lines.append("</head>")
    lines.append("<body>")
    lines.append("  <h1>Pulse Performance History</h1>")
    lines.append(
        '  <p class="muted">Generated from <code>perf-history.jsonl</code>. Trend charts are rendered per scenario.</p>'
    )
    lines.append('  <section class="meta">')
    lines.append(f'    <div class="card"><strong>Runs</strong><br>{run_count}</div>')
    lines.append(
        f'    <div class="card"><strong>Scenarios</strong><br>{len(all_scenarios)}</div>'
    )
    lines.append(
        f'    <div class="card"><strong>Latest Run</strong><br><code>{latest_run_id}</code></div>'
    )
    lines.append(
        f'    <div class="card"><strong>Latest Timestamp (UTC)</strong><br>{latest_timestamp}</div>'
    )
    lines.append(
        f'    <div class="card"><strong>Latest Overlay</strong><br>{latest_overlay}</div>'
    )
    lines.append("  </section>")

    if not records:
        lines.append("  <p>No history records found.</p>")
        lines.append("</body></html>")
        output_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return

    lines.append("  <h2>Scenarios</h2>")
    lines.append("  <ul>")
    for name in all_scenarios:
        slug = slugify(name)
        lines.append(f'    <li><a href="#{slug}">{name}</a></li>')
    lines.append("  </ul>")

    for name in all_scenarios:
        slug = slugify(name)
        points = collect_points(records, name, max_points)
        if not points:
            continue

        throughput_floor = latest_threshold(points, "throughput_floor")
        p95_max_s = latest_threshold(points, "p95_max_s")
        p99_max_s = latest_threshold(points, "p99_max_s")
        error_rate_max = latest_threshold(points, "error_rate_max")

        throughput_svg = output_file.parent / f"perf-history-{slug}-throughput.svg"
        latency_svg = output_file.parent / f"perf-history-{slug}-latency.svg"
        error_svg = output_file.parent / f"perf-history-{slug}-error-rate.svg"
        status_svg = output_file.parent / f"perf-history-{slug}-status.svg"

        write_line_chart(
            throughput_svg,
            f"{name} - Throughput (success_rate)",
            points,
            [("success_rate", "success_rate", "#2563eb")],
            [("floor", throughput_floor, "#dc2626")],
            "scenario/s",
        )
        write_line_chart(
            latency_svg,
            f"{name} - Latency (p95/p99)",
            points,
            [("p95_s", "p95_s", "#2563eb"), ("p99_s", "p99_s", "#d97706")],
            [("p95_max", p95_max_s, "#1d4ed8"), ("p99_max", p99_max_s, "#b45309")],
            "seconds",
        )
        write_line_chart(
            error_svg,
            f"{name} - Error Rate",
            points,
            [("error_rate", "error_rate", "#7c3aed")],
            [("max", error_rate_max, "#dc2626")],
            "ratio",
        )
        write_status_timeline(status_svg, f"{name} - Pass/Fail Timeline", points)

        latest_point = points[-1]
        status_css = "pass" if latest_point.get("status", "").upper() == "PASS" else "fail"
        lines.append(f'  <section class="scenario" id="{slug}">')
        lines.append(f"    <h3>{name}</h3>")
        lines.append("    <table>")
        lines.append("      <tr><th>Latest status</th><th>Latest run</th><th>Latest commit</th><th>Points rendered</th></tr>")
        lines.append(
            f'      <tr><td class="{status_css}">{latest_point.get("status", "UNKNOWN")}</td>'
            f'<td><code>{latest_point.get("run_id", "n/a")}</code></td>'
            f'<td><code>{latest_point.get("git_sha", "unknown")}</code></td>'
            f"<td>{len(points)}</td></tr>"
        )
        lines.append("    </table>")
        lines.append("    <table>")
        lines.append("      <tr><th>Metric</th><th>Latest</th><th>Threshold (latest)</th></tr>")
        lines.append(
            f"      <tr><td>success_rate</td><td>{fmt_num(to_float(latest_point.get('success_rate')))}</td><td>{fmt_num(throughput_floor)}</td></tr>"
        )
        lines.append(
            f"      <tr><td>p95_s</td><td>{fmt_num(to_float(latest_point.get('p95_s')))}</td><td>{fmt_num(p95_max_s)}</td></tr>"
        )
        lines.append(
            f"      <tr><td>p99_s</td><td>{fmt_num(to_float(latest_point.get('p99_s')))}</td><td>{fmt_num(p99_max_s)}</td></tr>"
        )
        lines.append(
            f"      <tr><td>error_rate</td><td>{fmt_num(to_float(latest_point.get('error_rate')))}</td><td>{fmt_num(error_rate_max)}</td></tr>"
        )
        lines.append("    </table>")
        lines.append('    <div class="charts">')
        lines.append(f'      <img src="{throughput_svg.name}" alt="{name} throughput chart">')
        lines.append(f'      <img src="{latency_svg.name}" alt="{name} latency chart">')
        lines.append(f'      <img src="{error_svg.name}" alt="{name} error-rate chart">')
        lines.append(f'      <img src="{status_svg.name}" alt="{name} status timeline">')
        lines.append("    </div>")
        lines.append("  </section>")

    lines.append("</body>")
    lines.append("</html>")
    output_file.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    history_file = Path(args.history_file)
    output_file = Path(args.output_file)

    records = load_history(history_file)
    build_history_page(records, output_file, args.max_points)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
