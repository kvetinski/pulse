#!/usr/bin/env python3
"""Verify and narrate cluster-wide Prometheus evidence for the demo."""

from __future__ import annotations

import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Dict, List, Tuple


SAMPLE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+([-+0-9.eE]+)$")
LABEL = re.compile(r'(\w+)="((?:\\.|[^"])*)"')
Samples = Dict[str, List[Tuple[Dict[str, str], float]]]


def fail(message: str) -> None:
    raise SystemExit(f"demo metrics verification failed: {message}")


def parse_samples(raw: str) -> Samples:
    samples: Samples = defaultdict(list)
    for line in raw.splitlines():
        match = SAMPLE.match(line.strip())
        if not match:
            continue
        name, raw_labels, raw_value = match.groups()
        labels = {key: value for key, value in LABEL.findall(raw_labels or "")}
        samples[name].append((labels, float(raw_value)))
    return samples


def total(samples: Samples, name: str, **wanted: str) -> int:
    value = sum(
        sample
        for labels, sample in samples.get(name, [])
        if all(labels.get(key) == expected for key, expected in wanted.items())
    )
    return int(value)


def load_nodes(arguments: list[str]) -> dict[str, Samples]:
    if len(arguments) != 2:
        fail("usage: verify_metrics.py NODE=METRICS_FILE NODE=METRICS_FILE")
    nodes: dict[str, Samples] = {}
    for argument in arguments:
        if "=" not in argument:
            fail(f"metric input has no node prefix: {argument}")
        node, path = argument.split("=", 1)
        if not node or node in nodes:
            fail(f"invalid or duplicate node name: {node!r}")
        nodes[node] = parse_samples(Path(path).read_text())
    return nodes


def cluster_total(nodes: dict[str, Samples], name: str, **wanted: str) -> int:
    return sum(total(samples, name, **wanted) for samples in nodes.values())


def main(arguments: list[str]) -> None:
    nodes = load_nodes(arguments)
    leaders = {
        node: total(samples, "pulse_scheduler_is_leader")
        for node, samples in nodes.items()
    }
    if sum(leaders.values()) != 1 or any(value not in {0, 1} for value in leaders.values()):
        fail(f"expected exactly one observed leader, found {leaders}")

    observed = {
        "scheduler jobs": cluster_total(nodes, "pulse_scheduler_jobs_published_total"),
        "physical deliveries": cluster_total(nodes, "pulse_worker_jobs_received_total"),
        "source commits": cluster_total(nodes, "pulse_worker_job_commits_total"),
        "durable job duplicates": cluster_total(
            nodes, "pulse_worker_jobs_duplicate_total"
        ),
        "results": cluster_total(nodes, "pulse_worker_results_published_total"),
        "automatic retries": cluster_total(nodes, "pulse_worker_retry_jobs_published_total"),
        "DLQ records": cluster_total(nodes, "pulse_worker_dlq_published_total"),
        "uncommitted jobs": cluster_total(nodes, "pulse_worker_uncommitted_jobs"),
        "retry queue": cluster_total(nodes, "pulse_worker_retry_queue_depth"),
        "commit failures": cluster_total(nodes, "pulse_worker_job_commit_failures_total"),
        "result publication failures": cluster_total(
            nodes, "pulse_worker_result_publish_failures_total"
        ),
        "complete aggregates": cluster_total(
            nodes, "pulse_aggregate_results_total", outcome="complete"
        ),
    }
    requirements = {
        "scheduler jobs": 3,
        "results": 3,
        "automatic retries": 0,
        "DLQ records": 0,
        "uncommitted jobs": 0,
        "retry queue": 0,
        "commit failures": 0,
        "result publication failures": 0,
        "complete aggregates": 2,
    }
    for name, required in requirements.items():
        if observed[name] != required:
            fail(f"{name}: expected {required}, observed {observed[name]}")
    logical_jobs = 3
    physical_deliveries = observed["physical deliveries"]
    if physical_deliveries < logical_jobs:
        fail(
            f"physical deliveries: expected at least {logical_jobs}, "
            f"observed {physical_deliveries}"
        )
    if observed["source commits"] != physical_deliveries:
        fail(
            "every physical delivery must have a terminal commit: "
            f"deliveries={physical_deliveries}, commits={observed['source commits']}"
        )
    expected_duplicates = physical_deliveries - logical_jobs
    if observed["durable job duplicates"] != expected_duplicates:
        fail(
            "redelivery count does not equal Redis-verified duplicate outcomes: "
            f"redeliveries={expected_duplicates}, "
            f"durable_duplicates={observed['durable job duplicates']}"
        )

    first = True
    for node in sorted(nodes):
        samples = nodes[node]
        role = "LEADER" if leaders[node] == 1 else "FOLLOWER"
        jobs = total(samples, "pulse_worker_jobs_received_total")
        commits = total(samples, "pulse_worker_job_commits_total")
        duplicates = total(samples, "pulse_worker_jobs_duplicate_total")
        label = "CLUSTER" if first else ""
        print(
            f"{label:<10} {node} [{role}] physical_deliveries={jobs} "
            f"commits={commits} durable_duplicates={duplicates}"
        )
        first = False
    print(
        f"SETTLE     logical jobs={logical_jobs}, physical deliveries={physical_deliveries}, "
        f"source commits={observed['source commits']}, uncommitted=0"
    )
    print(
        f"DEDUP      safe job redeliveries={expected_duplicates}; "
        "publication/commit failures=0"
    )
    print("POLICY     target failures caused automatic retries=0 and DLQ records=0")
    print("BOUNDS     retry queue=0; all original work reached a terminal disposition")


if __name__ == "__main__":
    main(sys.argv[1:])
