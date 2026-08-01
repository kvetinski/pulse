#!/usr/bin/env python3
"""Execute the per-node metrics verifier on the minimum valid cluster snapshot."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "verify_metrics.py"


LEADER_METRICS = """\
pulse_scheduler_is_leader 1
pulse_scheduler_jobs_published_total{scenario="healthy"} 2
pulse_scheduler_jobs_published_total{scenario="failed"} 1
pulse_worker_jobs_received_total 2
pulse_worker_job_commits_total 2
pulse_worker_jobs_duplicate_total 0
pulse_worker_results_published_total{scenario="healthy",status="success"} 2
pulse_aggregate_results_total{outcome="complete"} 2
pulse_aggregate_results_total{outcome="duplicate"} 1
"""

FOLLOWER_METRICS = """\
pulse_scheduler_is_leader 0
pulse_worker_jobs_received_total 2
pulse_worker_job_commits_total 2
pulse_worker_jobs_duplicate_total 1
pulse_worker_results_published_total{scenario="failed",status="failure"} 1
"""


class MetricsVerifierTests(unittest.TestCase):
    def invoke(
        self,
        leader: str = LEADER_METRICS,
        follower: str = FOLLOWER_METRICS,
    ) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as directory:
            first = Path(directory) / "pulse-a.prom"
            second = Path(directory) / "pulse-b.prom"
            first.write_text(leader)
            second.write_text(follower)
            return subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    f"pulse-demo-a={first}",
                    f"pulse-demo-b={second}",
                ],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_cluster_totals_and_roles_pass(self) -> None:
        completed = self.invoke()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("pulse-demo-a [LEADER]", completed.stdout)
        self.assertIn("pulse-demo-b [FOLLOWER]", completed.stdout)
        self.assertIn("physical deliveries=4", completed.stdout)
        self.assertIn("source commits=4", completed.stdout)
        self.assertIn("safe job redeliveries=1", completed.stdout)

    def test_two_leaders_fail_closed(self) -> None:
        completed = self.invoke(LEADER_METRICS.replace("is_leader 1", "is_leader 2"))
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("expected exactly one observed leader", completed.stderr)

    def test_unsettled_physical_delivery_fails_closed(self) -> None:
        completed = self.invoke(
            follower=FOLLOWER_METRICS.replace(
                "pulse_worker_job_commits_total 2",
                "pulse_worker_job_commits_total 1",
            )
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("every physical delivery must have a terminal commit", completed.stderr)

    def test_missing_durable_duplicate_evidence_fails_closed(self) -> None:
        completed = self.invoke(
            follower=FOLLOWER_METRICS.replace(
                "pulse_worker_jobs_duplicate_total 1",
                "pulse_worker_jobs_duplicate_total 0",
            )
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Redis-verified duplicate outcomes", completed.stderr)


if __name__ == "__main__":
    unittest.main()
