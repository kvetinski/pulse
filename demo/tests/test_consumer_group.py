#!/usr/bin/env python3
"""Regression tests for the demo consumer-group catch-up gate."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check_consumer_group.py"
TOPIC = "pulse.demo.results"
HEADER = "GROUP TOPIC PARTITION CURRENT-OFFSET LOG-END-OFFSET LAG CONSUMER-ID HOST CLIENT-ID\n"


class ConsumerGroupTests(unittest.TestCase):
    def invoke(self, rows: str, topic: str = TOPIC) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory) / "group.txt"
            snapshot.write_text(HEADER + rows)
            return subprocess.run(
                [sys.executable, str(SCRIPT), str(snapshot), topic],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_empty_uninitialized_partition_is_caught_up(self) -> None:
        completed = self.invoke(
            f"demo {TOPIC} 0 - 0 - consumer host client\n"
            f"demo {TOPIC} 1 2 2 0 consumer host client\n"
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_nonempty_uninitialized_partition_is_not_caught_up(self) -> None:
        completed = self.invoke(f"demo {TOPIC} 0 - 1 - consumer host client\n")
        self.assertEqual(completed.returncode, 1)

    def test_numeric_lag_is_not_caught_up(self) -> None:
        completed = self.invoke(f"demo {TOPIC} 0 1 2 1 consumer host client\n")
        self.assertEqual(completed.returncode, 1)

    def test_missing_topic_fails_closed(self) -> None:
        completed = self.invoke("demo another.topic 0 0 0 0 consumer host client\n")
        self.assertEqual(completed.returncode, 1)


if __name__ == "__main__":
    unittest.main()
