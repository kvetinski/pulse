#!/usr/bin/env python3
"""Tests for logical Kafka-record selection under duplicate publication."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "select_logical_records.py"


def summary(event_id: str, scenario_id: str) -> dict:
    return {
        "schema_version": 2,
        "event_id": event_id,
        "revision": 1,
        "summary": {"scenario_id": scenario_id},
    }


class LogicalRecordTests(unittest.TestCase):
    def invoke(
        self,
        records: list[dict],
        expected: int,
        identity_field: str = "event_id",
        record_name: str = "summaries",
    ) -> tuple[subprocess.CompletedProcess, str]:
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "physical.jsonl"
            destination = Path(directory) / "logical.jsonl"
            source.write_text("".join(json.dumps(record) + "\n" for record in records))
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    str(source),
                    str(destination),
                    str(expected),
                    identity_field,
                    record_name,
                ],
                check=False,
                capture_output=True,
                text=True,
            )
            output = destination.read_text() if destination.exists() else ""
        return completed, output

    def test_identical_physical_redeliveries_become_two_logical_events(self) -> None:
        healthy = summary("healthy:r1", "LocalUnaryHealthy")
        failed = summary("failed:r1", "LocalUnaryTargetFailure")
        completed, output = self.invoke(
            [failed, failed.copy(), healthy, healthy.copy()], expected=2
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Kafka records=4", completed.stdout)
        self.assertIn("logical summaries=2", completed.stdout)
        self.assertIn("identical copies=2", completed.stdout)
        self.assertEqual(len(output.splitlines()), 2)

    def test_same_event_id_with_different_payload_fails_closed(self) -> None:
        first = summary("same:r1", "LocalUnaryHealthy")
        second = summary("same:r1", "LocalUnaryTargetFailure")
        completed, _ = self.invoke([first, second], expected=1)
        self.assertEqual(completed.returncode, 2)
        self.assertIn("reused for different summaries payloads", completed.stderr)

    def test_missing_logical_event_is_retryable_for_the_shell_waiter(self) -> None:
        completed, _ = self.invoke(
            [summary("healthy:r1", "LocalUnaryHealthy")], expected=2
        )
        self.assertEqual(completed.returncode, 1)

    def test_jobs_can_use_execution_key_as_their_identity(self) -> None:
        first = {"execution_key": "job-a", "attempt": 0}
        second = {"execution_key": "job-b", "attempt": 0}
        completed, output = self.invoke(
            [first, first.copy(), second],
            expected=2,
            identity_field="execution_key",
            record_name="jobs",
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("logical jobs=2", completed.stdout)
        self.assertEqual(len(output.splitlines()), 2)

    def test_duplicate_cannot_hide_a_missing_logical_record(self) -> None:
        first = {"event_id": "result-a", "total": 1}
        completed, _ = self.invoke(
            [first, first.copy(), first.copy()],
            expected=3,
            record_name="results",
        )
        self.assertEqual(completed.returncode, 1)


if __name__ == "__main__":
    unittest.main()
