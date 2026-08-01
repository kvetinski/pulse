#!/usr/bin/env python3
"""Tests for duplicate-result summary revision verification."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "verify_replay.py"


def summary(event_id: str, total: int = 1) -> dict:
    return {
        "schema_version": 2,
        "event_id": event_id,
        "revision": 1,
        "summary": {"run_id": event_id.split(":summary", 1)[0], "total": total},
    }


class ReplayVerifierTests(unittest.TestCase):
    def invoke(self, before: list[dict], after: list[dict]) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as directory:
            before_path = Path(directory) / "before.jsonl"
            after_path = Path(directory) / "after.jsonl"
            before_path.write_text("".join(json.dumps(value) + "\n" for value in before))
            after_path.write_text("".join(json.dumps(value) + "\n" for value in after))
            return subprocess.run(
                [sys.executable, str(SCRIPT), str(before_path), str(after_path)],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_identical_outbox_copies_do_not_look_like_new_logical_revisions(self) -> None:
        healthy = summary("healthy:summary:r1:complete")
        failed = summary("failed:summary:r1:complete")
        completed = self.invoke(
            [healthy, failed],
            [healthy, failed, healthy.copy(), failed.copy()],
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("publication created no logical summary/revision", completed.stdout)
        self.assertIn("copies 2->4", completed.stdout)

    def test_new_summary_revision_fails(self) -> None:
        healthy = summary("healthy:summary:r1:complete")
        failed = summary("failed:summary:r1:complete")
        revision_two = summary("healthy:summary:r2:complete")
        revision_two["revision"] = 2
        completed = self.invoke([healthy, failed], [healthy, failed, revision_two])
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("created a logical run summary revision", completed.stderr)

    def test_same_event_id_with_changed_payload_fails(self) -> None:
        healthy = summary("healthy:summary:r1:complete")
        failed = summary("failed:summary:r1:complete")
        changed = summary("healthy:summary:r1:complete", total=2)
        completed = self.invoke([healthy, failed], [healthy, failed, changed])
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("reuses event_id", completed.stderr)


if __name__ == "__main__":
    unittest.main()
