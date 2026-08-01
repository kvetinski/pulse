#!/usr/bin/env python3
"""Validate and narrate the deterministic Kafka contracts emitted by the demo."""

from __future__ import annotations

import json
import math
import re
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


HEALTHY = "LocalUnaryHealthy"
TARGET_FAILURE = "LocalUnaryTargetFailure"


def fail(message: str) -> None:
    raise SystemExit(f"demo story verification failed: {message}")


def load_json_lines(path: str) -> list[dict[str, Any]]:
    records: list[dict[str, Any]] = []
    for line_number, raw in enumerate(Path(path).read_text().splitlines(), start=1):
        if not raw.strip():
            continue
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            fail(f"{path}:{line_number} is not JSON: {error}")
        if not isinstance(value, dict):
            fail(f"{path}:{line_number} is not a JSON object")
        records.append(value)
    return records


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


def grouped(records: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    output: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for record in records:
        scenario_id = record.get("scenario_id")
        require(isinstance(scenario_id, str), f"missing scenario_id in {record!r}")
        output[scenario_id].append(record)
    return output


def validate_jobs(jobs: list[dict[str, Any]]) -> dict[str, list[dict[str, Any]]]:
    require(len(jobs) == 3, f"expected 3 original slice jobs, found {len(jobs)}")
    by_scenario = grouped(jobs)
    require(set(by_scenario) == {HEALTHY, TARGET_FAILURE}, "unexpected job scenarios")
    require(len(by_scenario[HEALTHY]) == 2, "healthy plan must dispatch two slices")
    require(len(by_scenario[TARGET_FAILURE]) == 1, "failure plan must dispatch one slice")

    execution_keys: set[str] = set()
    for job in jobs:
        require(job.get("schema_version") == 2, "demo jobs must use contract v2")
        require(job.get("attempt") == 0, "the initial demo must contain no retry attempt")
        require(job.get("not_before_unix_ms") == 0, "initial jobs must not be deferred")
        execution_key = job.get("execution_key")
        require(isinstance(execution_key, str) and execution_key, "job execution_key is missing")
        require(execution_key not in execution_keys, "job execution identities are not unique")
        execution_keys.add(execution_key)

    healthy = by_scenario[HEALTHY]
    require(
        {job["slice"]["index"] for job in healthy} == {0, 1}
        and {job["slice"]["total"] for job in healthy} == {2},
        "healthy slice metadata is not exactly 0/2 and 1/2",
    )
    require(
        math.isclose(
            sum(job["load"]["scenarios_per_sec"] for job in healthy),
            12.0,
            rel_tol=0.0,
            abs_tol=1e-9,
        ),
        "healthy slice rates do not conserve the configured 12 SPS",
    )
    require(
        sum(job["load"]["max_concurrency"] for job in healthy) == 4,
        "healthy slice concurrency does not conserve the configured global value",
    )

    failed = by_scenario[TARGET_FAILURE][0]
    require(failed["slice"] == {"index": 0, "total": 1}, "failure slice metadata changed")
    return by_scenario


def validate_results(
    results: list[dict[str, Any]], jobs: list[dict[str, Any]]
) -> tuple[dict[str, list[dict[str, Any]]], int, int]:
    require(len(results) == 3, f"expected 3 slice results, found {len(results)}")
    job_keys = {job["execution_key"] for job in jobs}
    result_keys = {result.get("execution_key") for result in results}
    require(result_keys == job_keys, "result identities do not exactly match dispatched jobs")
    by_scenario = grouped(results)

    healthy_total = 0
    for result in by_scenario.get(HEALTHY, []):
        total = result.get("total")
        require(isinstance(total, int) and total > 0, "healthy slice measured no traffic")
        require(result.get("status") == "Success", "healthy target result did not succeed")
        require(result.get("success") == total and result.get("failure") == 0, "healthy counts drifted")
        require(not result.get("error_breakdown"), "healthy result unexpectedly contains errors")
        healthy_total += total

    failed_results = by_scenario.get(TARGET_FAILURE, [])
    require(len(failed_results) == 1, "expected one target-failure slice result")
    failed = failed_results[0]
    failure_total = failed.get("total")
    require(isinstance(failure_total, int) and failure_total > 0, "failure slice measured no traffic")
    require(failed.get("status") == "Failed", "gRPC Unavailable was not recorded as failure data")
    require(failed.get("success") == 0 and failed.get("failure") == failure_total, "failure counts drifted")
    errors = failed.get("error_breakdown", [])
    require(
        sum(error.get("count", 0) for error in errors) == failure_total
        and any(str(error.get("kind", "")).startswith("target_status:") for error in errors),
        "target failure breakdown is missing the gRPC status measurements",
    )
    return by_scenario, healthy_total, failure_total


def validate_summaries(
    events: list[dict[str, Any]],
    results_by_scenario: dict[str, list[dict[str, Any]]],
) -> dict[str, dict[str, Any]]:
    require(len(events) == 2, f"expected 2 run summaries, found {len(events)}")
    summaries: dict[str, dict[str, Any]] = {}
    for event in events:
        summary = event.get("summary")
        require(isinstance(summary, dict), "summary event is missing its summary payload")
        scenario_id = summary.get("scenario_id")
        require(scenario_id in {HEALTHY, TARGET_FAILURE}, "unexpected summary scenario")
        require(scenario_id not in summaries, "duplicate initial summary scenario")
        require(event.get("schema_version") == 2 and event.get("revision") == 1, "summary contract/revision changed")
        require(summary.get("status") == "Complete", "all expected slices did not aggregate")
        require(summary.get("missing_slices") == [], "complete summary reports missing slices")
        require(
            event.get("event_id") == f"{summary.get('run_id')}:summary:r1:complete",
            "summary event identity is not deterministic",
        )

        scenario_results = results_by_scenario[scenario_id]
        require(
            summary.get("total") == sum(result["total"] for result in scenario_results)
            and summary.get("success") == sum(result["success"] for result in scenario_results)
            and summary.get("failure") == sum(result["failure"] for result in scenario_results),
            f"{scenario_id} aggregate counts do not equal its slice results",
        )
        require(
            sum(bucket.get("count", 0) for bucket in summary.get("latency_histogram", []))
            == summary.get("total"),
            f"{scenario_id} mergeable histogram count does not equal total",
        )
        summaries[scenario_id] = summary

    require(
        summaries[HEALTHY].get("expected_slices") == 2
        and summaries[HEALTHY].get("received_slices") == 2,
        "healthy run did not merge both expected slices",
    )
    require(
        summaries[TARGET_FAILURE].get("expected_slices") == 1
        and summaries[TARGET_FAILURE].get("received_slices") == 1,
        "target-failure run did not settle its expected slice",
    )
    return summaries


def short(value: str, maximum: int = 76) -> str:
    return value if len(value) <= maximum else f"{value[: maximum - 3]}..."


def event_timestamp(value: str) -> str:
    value = re.sub(r"(\.\d{6})\d+", r"\1", value)
    normalized = f"{value[:-1]}+00:00" if value.endswith("Z") else value
    observed = datetime.fromisoformat(normalized)
    return f"{observed.strftime('%H:%M:%S.%f')[:-3]}Z"


def result_timestamp(value: Any) -> str:
    require(isinstance(value, int) and value > 0, "result timestamp is missing")
    observed = datetime.fromtimestamp(value / 1000, tz=timezone.utc)
    return f"{observed.strftime('%H:%M:%S.%f')[:-3]}Z"


def validate_runtime_events(
    events: list[dict[str, Any]],
    jobs: list[dict[str, Any]],
    results: list[dict[str, Any]],
    healthy_total: int,
    failure_total: int,
) -> tuple[
    dict[str, dict[str, Any]],
    list[dict[str, Any]],
    str,
    dict[str, int],
]:
    ready_nodes = {
        item.get("node") for item in events if item.get("kind") == "node_ready"
    }
    require(
        ready_nodes == {"pulse-demo-a", "pulse-demo-b"},
        f"both Pulse nodes did not appear ready in the event ledger: {ready_nodes}",
    )
    aggregate_nodes = {
        item.get("node")
        for item in events
        if item.get("kind") == "aggregator_ready"
    }
    require(
        aggregate_nodes == {"pulse-demo-a", "pulse-demo-b"},
        "both Pulse result aggregators did not appear ready in the event ledger: "
        f"{aggregate_nodes}",
    )
    leaders = {
        item.get("node") for item in events if item.get("kind") == "leader_acquired"
    }
    require(len(leaders) == 1, f"expected one observed scheduler leader, found {leaders}")

    job_keys = {job["execution_key"] for job in jobs}
    result_keys = {result["execution_key"] for result in results}
    require(job_keys == result_keys, "runtime correlation received mismatched job/result identities")

    def unique_by_execution(kind: str) -> dict[str, dict[str, Any]]:
        matching = [item for item in events if item.get("kind") == kind]
        output: dict[str, dict[str, Any]] = {}
        for item in matching:
            execution_key = item.get("execution_key")
            require(
                execution_key in job_keys,
                f"{kind} has unknown execution identity {execution_key!r}",
            )
            require(execution_key not in output, f"duplicate {kind} for {execution_key}")
            output[execution_key] = item
        require(set(output) == job_keys, f"{kind} does not cover every dispatched slice")
        return output

    dispatched = unique_by_execution("dispatch_ack")
    require(
        all(item.get("node") in leaders for item in dispatched.values()),
        "a non-leader node reported a scheduler dispatch acknowledgement",
    )
    leased = unique_by_execution("lease_acquired")
    terminal = unique_by_execution("terminal_result")

    consumed_events = [item for item in events if item.get("kind") == "job_consumed"]
    require(
        all(item.get("execution_key") in job_keys for item in consumed_events),
        "a physical job delivery has an unknown execution identity",
    )
    consumed_by_key: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for item in consumed_events:
        consumed_by_key[str(item["execution_key"])].append(item)
    require(
        set(consumed_by_key) == job_keys,
        "physical deliveries do not cover every dispatched slice",
    )

    commit_events = [item for item in events if item.get("kind") == "source_committed"]
    require(
        all(item.get("execution_key") in job_keys for item in commit_events),
        "a source commit has an unknown execution identity",
    )
    result_commits: dict[str, dict[str, Any]] = {}
    duplicate_commits: list[dict[str, Any]] = []
    for item in commit_events:
        execution_key = str(item["execution_key"])
        disposition = item.get("disposition")
        if disposition == "ResultPublished":
            require(
                execution_key not in result_commits,
                f"multiple ResultPublished commits for {execution_key}",
            )
            result_commits[execution_key] = item
        elif disposition == "DuplicateCompleted":
            duplicate_commits.append(item)
        else:
            fail(f"unexpected demo source disposition {disposition!r}")
    require(
        set(result_commits) == job_keys,
        "durable ResultPublished commits do not cover every logical job",
    )

    def coordinate(item: dict[str, Any]) -> tuple[Any, ...]:
        return (
            item.get("node"),
            item.get("execution_key"),
            item.get("kafka_topic"),
            item.get("kafka_partition"),
            item.get("kafka_offset"),
            item.get("attempt"),
        )

    require(
        Counter(map(coordinate, consumed_events)) == Counter(map(coordinate, commit_events)),
        "physical Kafka deliveries and terminal source commits do not match one-for-one",
    )

    def instant(item: dict[str, Any]) -> datetime:
        value = item.get("timestamp")
        require(isinstance(value, str), f"event has no timestamp: {item!r}")
        value = re.sub(r"(\.\d{6})\d+", r"\1", value)
        normalized = f"{value[:-1]}+00:00" if value.endswith("Z") else value
        return datetime.fromisoformat(normalized)

    primary_consumed: dict[str, dict[str, Any]] = {}
    extra_consumed: list[dict[str, Any]] = []
    for execution_key in job_keys:
        lease = leased[execution_key]
        result = terminal[execution_key]
        commit = result_commits[execution_key]
        candidates = [
            item
            for item in consumed_by_key[execution_key]
            if coordinate(item) == coordinate(commit) and instant(item) <= instant(lease)
        ]
        require(candidates, f"no primary consume precedes the lease for {execution_key}")
        primary = min(candidates, key=instant)
        primary_consumed[execution_key] = primary
        extra_consumed.extend(
            item for item in consumed_by_key[execution_key] if item is not primary
        )
        require(
            primary.get("node") == lease.get("node")
            == result.get("node")
            == commit.get("node"),
            f"primary execution correlation changed node for {execution_key}",
        )
        require(
            instant(primary) <= instant(lease) <= instant(result) <= instant(commit),
            f"primary consume/lease/result/commit order is invalid for {execution_key}",
        )
        require(
            commit.get("kafka_partition") == primary.get("kafka_partition")
            and commit.get("kafka_offset") == primary.get("kafka_offset"),
            f"source commit evidence does not follow the durable result for {execution_key}",
        )

    duplicate_events = [
        item for item in events if item.get("kind") == "duplicate_completed"
    ]
    require(
        len(extra_consumed) == len(duplicate_commits) == len(duplicate_events),
        "every redelivery must have one Redis duplicate verification and DuplicateCompleted commit",
    )
    require(
        all(
            item.get("execution_key") in job_keys
            and item.get("terminal_outcome") == "ResultPublished"
            for item in duplicate_events
        ),
        "duplicate verification does not reference a durable result outcome",
    )

    duplicate_groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    duplicate_commit_groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    extra_consume_groups: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
    for item in duplicate_events:
        duplicate_groups[(item.get("node"), item.get("execution_key"), item.get("attempt"))].append(item)
    for item in duplicate_commits:
        duplicate_commit_groups[(item.get("node"), item.get("execution_key"), item.get("attempt"))].append(item)
    for item in extra_consumed:
        extra_consume_groups[(item.get("node"), item.get("execution_key"), item.get("attempt"))].append(item)
    require(
        set(extra_consume_groups) == set(duplicate_groups) == set(duplicate_commit_groups),
        "redelivery, Redis dedupe and duplicate-commit correlation fields disagree",
    )
    for key in extra_consume_groups:
        deliveries = sorted(extra_consume_groups[key], key=instant)
        verifications = sorted(duplicate_groups[key], key=instant)
        commits = sorted(duplicate_commit_groups[key], key=instant)
        require(
            len(deliveries) == len(verifications) == len(commits),
            f"redelivery settlement counts disagree for {key}",
        )
        for delivery, verification, commit in zip(deliveries, verifications, commits):
            require(
                instant(delivery) <= instant(verification) <= instant(commit),
                f"redelivery was not verified before commit for {key}",
            )

    target_successes = [item for item in events if item.get("kind") == "target_success"]
    require(
        len(target_successes) == healthy_total,
        f"expected {healthy_total} healthy target calls, found {len(target_successes)}",
    )

    failure_job = next(job for job in jobs if job["scenario_id"] == TARGET_FAILURE)
    failure_worker = leased[failure_job["execution_key"]].get("node")
    target_failures = [
        item
        for item in events
        if item.get("kind") == "target_failure"
        and item.get("message") == "pulse-expected-target-failure"
    ]
    require(
        len(target_failures) == failure_total,
        f"expected {failure_total} timestamped target failures, found {len(target_failures)}",
    )
    measured = [
        item
        for item in events
        if item.get("kind") == "failure_measured"
        and item.get("scenario_id") == TARGET_FAILURE
    ]
    require(
        len(measured) == failure_total,
        f"expected {failure_total} Pulse failure measurements, found {len(measured)}",
    )
    require(
        all(
            item.get("node") == failure_worker
            and item.get("step") == "grpc:pulse.demo.v1.DemoService/Echo"
            and str(item.get("grpc_status", "")).lower() == "unavailable"
            for item in measured
        ),
        "failure measurements do not identify the handling node, step and gRPC status",
    )
    require(
        all(
            item.get("node") == "grpc-target"
            and item.get("method") == "pulse.demo.v1.DemoService/Echo"
            and isinstance(item.get("timestamp"), str)
            for item in target_failures
        ),
        "target-side failure evidence is missing its node, method or timestamp",
    )
    lease_waits = [item for item in events if item.get("kind") == "lease_busy"]
    require(
        all(item.get("execution_key") in job_keys for item in lease_waits),
        "lease contention references an unknown execution identity",
    )
    return (
        primary_consumed,
        sorted(target_failures, key=lambda item: item["timestamp"]),
        str(leaders.pop()),
        {
            "logical_jobs": len(job_keys),
            "physical_deliveries": len(consumed_events),
            "redeliveries": len(extra_consumed),
            "lease_waits": len(lease_waits),
        },
    )


def main() -> None:
    if len(sys.argv) != 5:
        fail(
            "usage: verify_story.py JOBS_JSONL RESULTS_JSONL SUMMARIES_JSONL EVENTS_JSONL"
        )
    jobs = load_json_lines(sys.argv[1])
    results = load_json_lines(sys.argv[2])
    summaries_raw = load_json_lines(sys.argv[3])
    events = load_json_lines(sys.argv[4])

    jobs_by_scenario = validate_jobs(jobs)
    results_by_scenario, healthy_total, failure_total = validate_results(results, jobs)
    summaries = validate_summaries(summaries_raw, results_by_scenario)
    consumed, target_failures, leader, delivery = validate_runtime_events(
        events, jobs, results, healthy_total, failure_total
    )

    healthy_run = jobs_by_scenario[HEALTHY][0]["run_id"]
    failure_run = jobs_by_scenario[TARGET_FAILURE][0]["run_id"]
    healthy_summary = summaries[HEALTHY]
    failed_summary = summaries[TARGET_FAILURE]
    result_by_key = {result["execution_key"]: result for result in results}

    print("PLAN       healthy: 12 SPS x 2s, concurrency 4 -> 2 deterministic slices")
    print("           failure:  2 SPS x 2s, concurrency 2 -> gRPC Unavailable")
    print(f"LEADER     {leader}; one fenced scheduler, two eligible workers")
    print(f"DISPATCH   healthy run={short(healthy_run)} slices=2/2 acknowledged")
    print(f"           failure run={short(failure_run)} slices=1/1 acknowledged")
    print(
        "DELIVERY   "
        f"logical jobs={delivery['logical_jobs']}, "
        f"physical deliveries={delivery['physical_deliveries']}, "
        f"safely settled redeliveries={delivery['redeliveries']}"
    )
    if delivery["lease_waits"]:
        print(
            "LEASE      "
            f"observed {delivery['lease_waits']} ownership conflict(s); "
            "source stayed uncommitted until Redis exposed a terminal outcome"
        )
    if delivery["redeliveries"]:
        print(
            "DEDUP      "
            f"Redis verified {delivery['redeliveries']} durable job outcome(s); "
            "extra target executions/results=0"
        )
    else:
        print(
            "DEDUP      no job redelivery occurred; target and result counts "
            "exactly match the logical plan"
        )
    ordered_jobs = sorted(
        jobs,
        key=lambda job: (
            job["scenario_id"] != HEALTHY,
            job["slice"]["index"],
        ),
    )
    for index, job in enumerate(ordered_jobs):
        source = consumed[job["execution_key"]]
        result = result_by_key[job["execution_key"]]
        label = "ROUTE" if index == 0 else ""
        print(
            f"{label:<10} jobs[p{source['kafka_partition']}/o{source['kafka_offset']}]"
            f" -> {source['node']} -> Redis lease -> gRPC target"
            f" -> result {result['status']} ({result['success']}/{result['total']} ok)"
        )
    print(f"TARGET     observed {healthy_total} successful unary calls")
    print(f"           observed {failure_total} expected gRPC Unavailable measurements")
    failure_result = results_by_scenario[TARGET_FAILURE][0]
    first_failure = event_timestamp(target_failures[0]["timestamp"])
    last_failure = event_timestamp(target_failures[-1]["timestamp"])
    failure_source = consumed[failure_result["execution_key"]]
    print(
        f"FAILURE    {first_failure}..{last_failure} at grpc-target / "
        "pulse.demo.v1.DemoService/Echo"
    )
    print(
        f"           {failure_source['node']} handled jobs"
        f"[p{failure_source['kafka_partition']}/o{failure_source['kafka_offset']}] "
        f"slice={failure_source['slice']} attempt={failure_source['attempt']}"
    )
    print(
        "           slice execution window "
        f"{result_timestamp(failure_result['started_at_unix_ms'])}.."
        f"{result_timestamp(failure_result['finished_at_unix_ms'])}; "
        "classified target_status:Unavailable"
    )
    print("RESULT     3 deterministic slice events durably reached Kafka")
    print(
        "SUMMARY    healthy Complete 2/2 "
        f"p50={healthy_summary['scenario_latency_p50_ms']}ms "
        f"p95={healthy_summary['scenario_latency_p95_ms']}ms"
    )
    print(
        "           failure Complete 1/1 "
        f"total={failed_summary['total']} failure={failed_summary['failure']}"
    )
    print("           ('Complete' means all slices arrived, not that target calls succeeded)")


if __name__ == "__main__":
    main()
