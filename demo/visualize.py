#!/usr/bin/env python3
"""Render verified Pulse demo logs as a terminal topology and event ledger."""

from __future__ import annotations

import argparse
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path
from typing import Any


ANSI = re.compile(r"\x1b\[[0-9;]*m")
FIELD = r"\b{field}=(?:\"([^\"]*)\"|([^\s]+))"
OUTER_TIMESTAMP = re.compile(r"\|\s+(\d{4}-\d{2}-\d{2}T[^\s]+Z)\s+")


def plain(value: str) -> str:
    return ANSI.sub("", value)


def field(value: str, name: str) -> str | None:
    match = re.search(FIELD.format(field=re.escape(name)), value)
    if not match:
        return None
    return match.group(1) if match.group(1) is not None else match.group(2)


def service_name(line: str) -> str:
    prefix = line.split("|", 1)[0].strip()
    if "-pulse-a-" in prefix:
        return "pulse-demo-a"
    if "-pulse-b-" in prefix:
        return "pulse-demo-b"
    if "-grpc-target-" in prefix:
        return "grpc-target"
    return prefix or "unknown"


def timestamp(line: str) -> str | None:
    match = OUTER_TIMESTAMP.search(line)
    return match.group(1) if match else None


def parse_timestamp(value: str) -> datetime:
    # Docker emits RFC3339Nano (nine fractional digits), while Python 3.8's
    # fromisoformat accepts at most microseconds. Display precision is already
    # milliseconds, so truncate rather than round across an event boundary.
    value = re.sub(r"(\.\d{6})\d+", r"\1", value)
    normalized = f"{value[:-1]}+00:00" if value.endswith("Z") else value
    return datetime.fromisoformat(normalized)


def event(kind: str, line: str, **details: Any) -> dict[str, Any]:
    return {
        "kind": kind,
        "timestamp": timestamp(line),
        "node": service_name(line),
        **{key: value for key, value in details.items() if value is not None},
    }


def parse_event(raw: str) -> dict[str, Any] | None:
    """Extract only stable, correlation-bearing events from one Compose log line."""

    line = plain(raw).strip()
    if "demo_target_listening" in line:
        return event("target_ready", line, address=field(line, "address"))
    if "Pulse runtime ready" in line:
        return event("node_ready", line)
    if "distributed result aggregator ready" in line:
        return event("aggregator_ready", line)
    if "leadership acquired" in line:
        return event(
            "leader_acquired",
            line,
            fence=field(line, "fence"),
            owner_token=field(line, "owner_token"),
        )
    if "slice publication acknowledged" in line:
        return event(
            "dispatch_ack",
            line,
            scenario_id=field(line, "scenario"),
            run_id=field(line, "run_id"),
            execution_key=field(line, "execution_key"),
            slice_index=field(line, "slice_index"),
            progress=field(line, "progress"),
        )
    if "job processing started" in line:
        return event(
            "job_consumed",
            line,
            scenario_id=field(line, "scenario_id"),
            run_id=field(line, "run_id"),
            execution_key=field(line, "execution_key"),
            slice=field(line, "slice"),
            attempt=field(line, "attempt"),
            kafka_topic=field(line, "kafka_topic"),
            kafka_partition=field(line, "kafka_partition"),
            kafka_offset=field(line, "kafka_offset"),
        )
    if "execution lease acquired" in line:
        return event(
            "lease_acquired",
            line,
            scenario_id=field(line, "scenario_id"),
            run_id=field(line, "run_id"),
            execution_key=field(line, "execution_key"),
            slice=field(line, "slice"),
            attempt=field(line, "attempt"),
            lease_owner=field(line, "lease_owner"),
            lease_recovered=field(line, "lease_recovered"),
        )
    if "execution lease is busy" in line:
        return event(
            "lease_busy",
            line,
            execution_key=field(line, "execution_key"),
            retry_after_ms=field(line, "retry_after_ms"),
        )
    if "verified durable duplicate" in line:
        return event(
            "duplicate_completed",
            line,
            execution_key=field(line, "execution_key"),
            attempt=field(line, "attempt"),
            terminal_outcome=field(line, "terminal_outcome"),
        )
    if "demo_target_request" in line:
        outcome = field(line, "outcome")
        return event(
            "target_failure" if outcome == "unavailable" else "target_success",
            line,
            sequence=field(line, "sequence"),
            method=field(line, "method") or "pulse.demo.v1.DemoService/Echo",
            message=field(line, "message"),
            delay_ms=field(line, "delay_ms"),
            outcome=outcome,
        )
    if "step execution failed" in line:
        status_match = re.search(r"status:\s*([A-Za-z]+)", line)
        return event(
            "failure_measured",
            line,
            scenario_id=field(line, "scenario"),
            step=field(line, "step"),
            grpc_status=status_match.group(1) if status_match else "unknown",
        )
    if "scenario slice reached durable terminal result" in line:
        return event(
            "terminal_result",
            line,
            scenario_id=field(line, "scenario"),
            run_id=field(line, "run_id"),
            execution_key=field(line, "execution_key"),
            attempt=field(line, "attempt"),
            status=field(line, "status"),
            started=field(line, "started"),
            finished=field(line, "finished"),
            peak_pending_tasks=field(line, "peak_pending_tasks"),
        )
    if "source offset committed after durable terminal disposition" in line:
        return event(
            "source_committed",
            line,
            scenario_id=field(line, "scenario_id"),
            run_id=field(line, "run_id"),
            execution_key=field(line, "execution_key"),
            slice=field(line, "slice"),
            attempt=field(line, "attempt"),
            kafka_topic=field(line, "kafka_topic"),
            kafka_partition=field(line, "kafka_partition"),
            kafka_offset=field(line, "kafka_offset"),
            disposition=field(line, "disposition"),
        )
    return None


def render_topology(unicode: bool = True) -> str:
    if not unicode:
        return """TOPOLOGY   two eligible Pulse replicas; Redis elects exactly one scheduler

  pulse-demo-a  <=== jobs / results ===>  KAFKA (3 partitions)  <=== jobs / results ===>  pulse-demo-b
       ^                                      summaries                                      ^
       |                                                                                     |
       +<=== leader fence / dispatch ledger / execution leases / aggregate ===> REDIS <======+
       \\--------------------- unary gRPC ----------------> TARGET <-------------------------/
                                                   DemoService/Echo"""

    return """TOPOLOGY   two eligible Pulse replicas; Redis elects exactly one scheduler

  pulse-demo-a  ◄── jobs / results ──►  KAFKA (3 partitions)  ◄── jobs / results ──►  pulse-demo-b
       ▲                                      summaries                                      ▲
       │                                                                                     │
       └◄── leader fence / dispatch ledger / execution leases / aggregate ──► REDIS ◄────────┘
        ╲──────────────────── unary gRPC ────────────────► TARGET ◄──────────────────────────╱
                                                   DemoService/Echo"""


class Palette:
    def __init__(self, enabled: bool) -> None:
        self.enabled = enabled

    def wrap(self, code: str, value: str) -> str:
        return f"\x1b[{code}m{value}\x1b[0m" if self.enabled else value

    def kind(self, kind: str, value: str) -> str:
        if kind in {"target_failure", "failure_measured", "lease_busy"}:
            return self.wrap("1;33", value)
        if kind in {
            "terminal_result",
            "dispatch_ack",
            "leader_acquired",
            "duplicate_completed",
            "source_committed",
        }:
            return self.wrap("1;32", value)
        return self.wrap("1;36", value)


LABELS = {
    "target_ready": "CONNECT",
    "node_ready": "READY",
    "aggregator_ready": "AGGREGATE",
    "leader_acquired": "LEADER",
    "dispatch_ack": "DISPATCH",
    "job_consumed": "CONSUME",
    "lease_acquired": "LEASE",
    "lease_busy": "LEASE WAIT",
    "duplicate_completed": "DEDUP",
    "target_success": "TARGET",
    "target_failure": "TARGET ERR",
    "failure_measured": "MEASURE",
    "terminal_result": "RESULT",
    "source_committed": "COMMIT",
}


def short(value: Any, maximum: int = 42) -> str:
    text = str(value) if value is not None else "?"
    return text if len(text) <= maximum else f"{text[: maximum - 3]}..."


def short_execution(value: Any) -> str:
    text = str(value) if value is not None else "?"
    parts = text.split(":")
    if len(parts) >= 3 and parts[-1].startswith("slice-"):
        return short(":".join(parts[-3:]), 38)
    return short(text, 38)


def short_method(value: Any) -> str:
    text = str(value) if value is not None else "?"
    if text.startswith("grpc:"):
        text = text[5:]
    if "/" not in text:
        return short(text, 32)
    service, method = text.rsplit("/", 1)
    return f"{service.rsplit('.', 1)[-1]}/{method}"


def short_scenario(value: Any) -> str:
    text = str(value) if value is not None else "?"
    if text.startswith("LocalUnary"):
        text = text[len("LocalUnary") :]
    return short(text, 24)


def event_detail(item: dict[str, Any]) -> str:
    kind = item["kind"]
    if kind == "target_ready":
        return f"listening at {item.get('address', '?')}"
    if kind == "node_ready":
        return "runtime dependencies initialized"
    if kind == "aggregator_ready":
        return "result consumer and durable summary outbox ready"
    if kind == "leader_acquired":
        return f"leadership claim ──► Redis; fence={item.get('fence', '?')}"
    if kind == "dispatch_ack":
        return (
            f"slice {item.get('slice_index', '?')} ──► Kafka ack ──► Redis; "
            f"{item.get('progress', '?')}; {short_scenario(item.get('scenario_id'))}"
        )
    if kind == "job_consumed":
        return (
            f"Kafka jobs[p{item.get('kafka_partition', '?')}/o{item.get('kafka_offset', '?')}] "
            f"──► worker; {short_scenario(item.get('scenario_id'))} "
            f"{item.get('slice', '?')}; exec={short_execution(item.get('execution_key'))}"
        )
    if kind == "lease_acquired":
        return (
            f"execution claim ──► Redis; exec={short_execution(item.get('execution_key'))} "
            f"recovered={item.get('lease_recovered', '?')}"
        )
    if kind == "lease_busy":
        return (
            f"claim ──► Redis BUSY; exec={short_execution(item.get('execution_key'))}; "
            f"TTL={item.get('retry_after_ms', '?')}ms; source remains uncommitted"
        )
    if kind == "duplicate_completed":
        return (
            f"Redis terminal ──► worker; exec={short_execution(item.get('execution_key'))} "
            f"outcome="
            f"{item.get('terminal_outcome', '?')} verified; no target re-execution"
        )
    if kind == "target_success":
        return (
            f"unary gRPC ◄── Pulse worker; {short_method(item.get('method'))} "
            f"request #{item.get('sequence', '?')} "
            f"OK ({item.get('delay_ms', '?')}ms fixture delay)"
        )
    if kind == "target_failure":
        return (
            f"unary gRPC ◄── Pulse worker; {short_method(item.get('method'))} "
            f"request #{item.get('sequence', '?')} × UNAVAILABLE"
        )
    if kind == "failure_measured":
        return (
            f"gRPC status ◄── target ×; {short_scenario(item.get('scenario_id'))} "
            f"{short_method(item.get('step'))} = {item.get('grpc_status', '?')}"
        )
    if kind == "terminal_result":
        return (
            f"Kafka result + Redis terminal; {short_scenario(item.get('scenario_id'))} "
            f"{item.get('status', '?')}; measured={item.get('finished', '?')}"
        )
    if kind == "source_committed":
        return (
            f"offset ──commit──► Kafka jobs"
            f"[p{item.get('kafka_partition', '?')}/o{item.get('kafka_offset', '?')}] "
            f"after {item.get('disposition', '?')}"
        )
    return kind


def should_display(item: dict[str, Any]) -> bool:
    if item["kind"] != "target_success":
        return True
    try:
        sequence = int(item.get("sequence", 0))
    except (TypeError, ValueError):
        return True
    # Show pacing without flooding the terminal with every healthy request.
    return sequence == 1 or sequence % 6 == 0


def format_time(item: dict[str, Any], origin: datetime | None) -> str:
    value = item.get("timestamp")
    if not value:
        return "--:--:--.---  +?.???s"
    observed = parse_timestamp(value)
    elapsed = (observed - origin).total_seconds() if origin else 0.0
    return f"{observed.strftime('%H:%M:%S.%f')[:-3]}Z  {elapsed:+07.3f}s"


def render_event(item: dict[str, Any], origin: datetime | None, palette: Palette) -> str:
    if item["kind"] == "job_consumed" and item.get("delivery_index", 1) > 1:
        label = "REDELIVER"
    else:
        label = LABELS.get(item["kind"], item["kind"].upper())
    label = palette.kind(item["kind"], f"{label:<10}")
    return (
        f"{format_time(item, origin)}  {label}  "
        f"{item.get('node', '?'):<12}  {event_detail(item)}"
    )


def annotate_deliveries(events: list[dict[str, Any]]) -> None:
    """Mark physical Kafka redeliveries without collapsing event evidence."""

    deliveries: dict[str, int] = {}
    for item in events:
        if item.get("kind") != "job_consumed":
            continue
        execution_key = str(item.get("execution_key"))
        delivery_index = deliveries.get(execution_key, 0) + 1
        deliveries[execution_key] = delivery_index
        item["delivery_index"] = delivery_index
        item["redelivery"] = delivery_index > 1


def observe(args: argparse.Namespace) -> int:
    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    runtime_log = evidence_dir / "runtime.log"
    events_log = evidence_dir / "events.jsonl"
    runtime_log.write_text("")
    events_log.write_text("")

    use_unicode = not args.ascii and os.environ.get("PULSE_DEMO_ASCII") != "1"
    use_color = sys.stdout.isatty() and "NO_COLOR" not in os.environ
    palette = Palette(use_color)
    print()
    print(render_topology(use_unicode))
    print()
    print(
        "STORY      healthy traffic plus one intentional target-error scenario; "
        "UNAVAILABLE below is expected evidence",
        flush=True,
    )
    print(
        f"OBSERVE    workload active; waiting for {args.expected_terminals} "
        "durable slice results...",
        flush=True,
    )
    sys.stdout.flush()

    command = [
        "docker",
        "compose",
        "--project-name",
        args.project_name,
        "--file",
        args.compose_file,
        "logs",
        "--follow",
        "--no-color",
        "--timestamps",
        "pulse-a",
        "pulse-b",
        "grpc-target",
    ]
    process = subprocess.Popen(  # noqa: S603 - fixed local demo command
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    assert process.stdout is not None

    lines: queue.Queue = queue.Queue()

    def read_lines() -> None:
        assert process.stdout is not None
        for raw_line in process.stdout:
            lines.put(raw_line)
        lines.put(None)

    reader = threading.Thread(target=read_lines, name="pulse-demo-log-reader", daemon=True)
    reader.start()

    started = time.monotonic()
    committed_keys: set[str] = set()
    completed_at: float | None = None
    parsed_events: list[dict[str, Any]] = []
    last_progress = started
    try:
        with runtime_log.open("a") as raw_output:
            while time.monotonic() - started < args.timeout_seconds:
                try:
                    raw = lines.get(timeout=0.25)
                except queue.Empty:
                    raw = ""
                if raw is None:
                    break
                if raw:
                    raw_output.write(plain(raw))
                    raw_output.flush()
                    item = parse_event(raw)
                    if item is not None:
                        parsed_events.append(item)
                        if (
                            item["kind"] == "source_committed"
                            and item.get("disposition") == "ResultPublished"
                        ):
                            committed_keys.add(str(item.get("execution_key")))
                            if len(committed_keys) >= args.expected_terminals:
                                completed_at = completed_at or time.monotonic()
                if (
                    completed_at is not None
                    and time.monotonic() - completed_at >= args.settlement_grace_seconds
                ):
                    break
                now = time.monotonic()
                if now - last_progress >= 3.0:
                    calls = sum(
                        item["kind"] in {"target_success", "target_failure"}
                        for item in parsed_events
                    )
                    failures = sum(
                        item["kind"] == "target_failure" for item in parsed_events
                    )
                    print(
                        f"OBSERVE    +{now - started:04.1f}s "
                        f"target_calls={calls} failures={failures} "
                        f"durable_commits={len(committed_keys)}/{args.expected_terminals}",
                        flush=True,
                    )
                    last_progress = now
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
        reader.join(timeout=1)

    if len(committed_keys) != args.expected_terminals:
        print(
            f"visual observer expected {args.expected_terminals} durably committed slices, "
            f"observed {len(committed_keys)}; see {runtime_log}",
            file=sys.stderr,
        )
        return 1

    parsed_events.sort(
        key=lambda item: (
            item.get("timestamp") or "~",
            item.get("node", ""),
            item.get("kind", ""),
        )
    )
    annotate_deliveries(parsed_events)
    with events_log.open("w") as event_output:
        for item in parsed_events:
            event_output.write(json.dumps(item, sort_keys=True) + "\n")

    runtime_ready = [
        item
        for item in parsed_events
        if item.get("kind") == "node_ready" and item.get("timestamp")
    ]
    timestamped = [item for item in parsed_events if item.get("timestamp")]
    origin_candidates = runtime_ready or timestamped
    origin_item = (
        min(origin_candidates, key=lambda item: item["timestamp"])
        if origin_candidates
        else None
    )
    origin = parse_timestamp(origin_item["timestamp"]) if origin_item else None
    print()
    print("EVENTS     chronological replay of the observed Pulse and target evidence")
    print("           UTC time       elapsed   event     node          evidence")
    print("           ───────────────────────────────────────────────────────────────────────────")
    for item in parsed_events:
        if should_display(item):
            print(render_event(item, origin, palette), flush=True)
            if args.replay_delay_ms > 0:
                time.sleep(args.replay_delay_ms / 1000)
    print("           ───────────────────────────────────────────────────────────────────────────")
    print(f"EVIDENCE   raw timestamped log: {runtime_log}")
    print(f"           parsed event ledger: {events_log}")
    return 0


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(description=__doc__)
    value.add_argument("--compose-file", required=True)
    value.add_argument("--project-name", default="pulse-demo")
    value.add_argument("--evidence-dir", required=True)
    value.add_argument("--expected-terminals", type=int, default=3)
    value.add_argument("--timeout-seconds", type=float, default=120.0)
    value.add_argument(
        "--settlement-grace-seconds",
        type=float,
        default=4.0,
        help="continue observing after logical commits to capture immediate redeliveries",
    )
    value.add_argument(
        "--replay-delay-ms",
        type=float,
        default=float(os.environ.get("PULSE_DEMO_REPLAY_DELAY_MS", "20")),
    )
    value.add_argument("--ascii", action="store_true")
    return value


if __name__ == "__main__":
    raise SystemExit(observe(parser().parse_args()))
