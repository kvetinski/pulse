#!/usr/bin/env python3
"""Deterministic tests for the demo's terminal topology and log parser."""

from __future__ import annotations

import sys
import unittest
from datetime import datetime, timezone
from pathlib import Path


DEMO_DIR = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(DEMO_DIR))

from visualize import (  # noqa: E402
    Palette,
    annotate_deliveries,
    event_detail,
    parse_event,
    render_event,
    render_topology,
    short_execution,
    short_method,
)


def log(service: str, message: str, timestamp: str = "2026-08-01T05:31:00.484246Z") -> str:
    return f"pulse-demo-{service}-1 | {timestamp} {message}"


class VisualizeTests(unittest.TestCase):
    def test_topology_names_every_node_and_connection_kind(self) -> None:
        topology = render_topology(unicode=False)
        for expected in (
            "pulse-demo-a",
            "pulse-demo-b",
            "KAFKA (3 partitions)",
            "REDIS",
            "TARGET",
            "leader fence",
            "execution leases",
            "unary gRPC",
        ):
            self.assertIn(expected, topology)

    def test_worker_event_keeps_node_and_kafka_coordinates(self) -> None:
        item = parse_event(
            log(
                "pulse-b",
                'INFO job processing started scenario_id="LocalUnaryTargetFailure" '
                'run_id="run-1" execution_key="execution-1" slice="0/1" attempt="0" '
                'kafka_topic="pulse.demo.jobs" kafka_partition=2 kafka_offset=41',
            )
        )
        self.assertIsNotNone(item)
        assert item is not None
        self.assertEqual(item["kind"], "job_consumed")
        self.assertEqual(item["node"], "pulse-demo-b")
        self.assertEqual(item["execution_key"], "execution-1")
        self.assertEqual(item["kafka_partition"], "2")
        self.assertEqual(item["kafka_offset"], "41")

    def test_target_and_measurement_failures_name_when_and_where(self) -> None:
        target = parse_event(
            log(
                "grpc-target",
                "demo_target_request sequence=25 "
                "method=pulse.demo.v1.DemoService/Echo instance=fixture-1 "
                'message="pulse-expected-target-failure" delay_ms=20 outcome=unavailable',
            )
        )
        measured = parse_event(
            log(
                "pulse-a",
                'ERROR step execution failed scenario=LocalUnaryTargetFailure '
                'step="grpc:pulse.demo.v1.DemoService/Echo" '
                'error=target status: Unavailable, message: "expected"',
                "2026-08-01T05:31:00.485000Z",
            )
        )
        self.assertEqual(target["kind"], "target_failure")
        self.assertEqual(target["node"], "grpc-target")
        self.assertEqual(target["outcome"], "unavailable")
        self.assertEqual(measured["kind"], "failure_measured")
        self.assertEqual(measured["node"], "pulse-demo-a")
        self.assertEqual(measured["grpc_status"], "Unavailable")
        self.assertEqual(
            measured["step"], "grpc:pulse.demo.v1.DemoService/Echo"
        )

    def test_commit_event_keeps_terminal_disposition_and_source_offset(self) -> None:
        item = parse_event(
            log(
                "pulse-b",
                'INFO source offset committed after durable terminal disposition '
                'scenario_id="LocalUnaryTargetFailure" run_id="run-1" '
                'execution_key="execution-1" slice="0/1" attempt="0" '
                'kafka_topic="pulse.demo.jobs" kafka_partition=2 kafka_offset=41 '
                "disposition=ResultPublished",
            )
        )
        self.assertEqual(item["kind"], "source_committed")
        self.assertEqual(item["node"], "pulse-demo-b")
        self.assertEqual(item["kafka_partition"], "2")
        self.assertEqual(item["kafka_offset"], "41")
        self.assertEqual(item["disposition"], "ResultPublished")

    def test_lease_contention_and_durable_duplicate_are_visible(self) -> None:
        busy = parse_event(
            log(
                "pulse-b",
                "INFO execution lease is busy execution_key=execution-1 "
                "retry_after_ms=19136",
            )
        )
        duplicate = parse_event(
            log(
                "pulse-b",
                "INFO verified durable duplicate execution_key=execution-1 "
                "attempt=0 terminal_outcome=ResultPublished",
            )
        )
        self.assertEqual(busy["kind"], "lease_busy")
        self.assertEqual(busy["retry_after_ms"], "19136")
        self.assertEqual(duplicate["kind"], "duplicate_completed")
        self.assertEqual(duplicate["terminal_outcome"], "ResultPublished")

    def test_second_physical_delivery_renders_as_redelivery(self) -> None:
        events = [
            parse_event(
                log(
                    "pulse-a",
                    "INFO job processing started scenario_id=LocalUnaryHealthy "
                    "run_id=run-1 execution_key=execution-1 slice=0/1 attempt=0 "
                    "kafka_topic=pulse.demo.jobs kafka_partition=1 kafka_offset=4",
                )
            ),
            parse_event(
                log(
                    "pulse-b",
                    "INFO job processing started scenario_id=LocalUnaryHealthy "
                    "run_id=run-1 execution_key=execution-1 slice=0/1 attempt=0 "
                    "kafka_topic=pulse.demo.jobs kafka_partition=1 kafka_offset=4",
                    "2026-08-01T05:31:01.000000Z",
                )
            ),
        ]
        parsed = [item for item in events if item is not None]
        annotate_deliveries(parsed)
        self.assertFalse(parsed[0]["redelivery"])
        self.assertTrue(parsed[1]["redelivery"])
        rendered = render_event(
            parsed[1],
            datetime(2026, 8, 1, 5, 31, tzinfo=timezone.utc),
            Palette(False),
        )
        self.assertIn("REDELIVER", rendered)
        self.assertIn("pulse-demo-b", rendered)

    def test_visual_identifiers_are_compact_but_keep_correlation_suffixes(self) -> None:
        execution = (
            "v2:s17:LocalUnaryHealthy:w1785566735115:n2:slice-1-of-2"
        )
        self.assertEqual(
            short_execution(execution),
            "w1785566735115:n2:slice-1-of-2",
        )
        self.assertEqual(
            short_method("grpc:pulse.demo.v1.DemoService/Echo"),
            "DemoService/Echo",
        )

    def test_edge_detail_does_not_repeat_the_node_column(self) -> None:
        item = parse_event(
            log(
                "pulse-b",
                "INFO leadership acquired fence=1 owner_token=owner-1",
            )
        )
        assert item is not None
        rendered = render_event(
            item,
            datetime(2026, 8, 1, 5, 31, tzinfo=timezone.utc),
            Palette(False),
        )
        self.assertEqual(rendered.count("pulse-demo-b"), 1)
        self.assertIn("leadership claim", rendered)

    def test_plain_event_renderer_has_no_terminal_control_sequences(self) -> None:
        item = parse_event(
            log(
                "grpc-target",
                "demo_target_request sequence=25 "
                "method=pulse.demo.v1.DemoService/Echo "
                'message="pulse-expected-target-failure" delay_ms=20 outcome=unavailable',
            )
        )
        assert item is not None
        rendered = render_event(
            item,
            datetime(2026, 8, 1, 5, 31, tzinfo=timezone.utc),
            Palette(False),
        )
        self.assertNotIn("\x1b[", rendered)
        self.assertIn("05:31:00.484Z", rendered)
        self.assertIn("+00.484s", rendered)
        self.assertIn("grpc-target", rendered)
        self.assertIn("UNAVAILABLE", rendered)
        self.assertIn("TARGET ERR", rendered)

    def test_runtime_and_aggregator_readiness_are_separate_events(self) -> None:
        runtime = parse_event(log("pulse-a", "INFO Pulse runtime ready"))
        aggregator = parse_event(
            log("pulse-a", "INFO distributed result aggregator ready")
        )
        assert runtime is not None
        assert aggregator is not None
        self.assertEqual(runtime["kind"], "node_ready")
        self.assertEqual(aggregator["kind"], "aggregator_ready")
        self.assertIn("dependencies initialized", event_detail(runtime))
        self.assertIn("summary outbox ready", event_detail(aggregator))

    def test_setup_events_can_render_before_runtime_origin(self) -> None:
        item = parse_event(
            log(
                "grpc-target",
                "INFO demo_target_listening address=0.0.0.0:50051",
                "2026-08-01T05:30:50.000000Z",
            )
        )
        assert item is not None
        rendered = render_event(
            item,
            datetime(2026, 8, 1, 5, 31, tzinfo=timezone.utc),
            Palette(False),
        )
        self.assertIn("-10.000s", rendered)

    def test_docker_nanosecond_timestamp_is_rendered_portably(self) -> None:
        item = parse_event(
            log(
                "pulse-a",
                "INFO Pulse runtime ready",
                "2026-08-01T05:31:00.484246917Z",
            )
        )
        assert item is not None
        rendered = render_event(
            item,
            datetime(2026, 8, 1, 5, 31, tzinfo=timezone.utc),
            Palette(False),
        )
        self.assertIn("05:31:00.484Z", rendered)
        self.assertIn("+00.484s", rendered)


if __name__ == "__main__":
    unittest.main()
