#!/usr/bin/env python3
"""Prove duplicate result ingestion did not create a logical summary revision."""

from __future__ import annotations

import json
import sys
from pathlib import Path


def fail(message: str) -> None:
    raise SystemExit(f"demo duplicate verification failed: {message}")


def logical_events(path: str) -> tuple[dict[str, str], int]:
    events: dict[str, str] = {}
    physical = 0
    for line_number, raw in enumerate(Path(path).read_text().splitlines(), start=1):
        if not raw.strip():
            continue
        physical += 1
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            fail(f"{path}:{line_number} is not JSON: {error}")
        event_id = value.get("event_id") if isinstance(value, dict) else None
        if not isinstance(event_id, str) or not event_id:
            fail(f"{path}:{line_number} has no deterministic event_id")
        canonical = json.dumps(value, sort_keys=True, separators=(",", ":"))
        previous = events.get(event_id)
        if previous is not None and previous != canonical:
            fail(f"{path}:{line_number} reuses event_id {event_id!r} with a new payload")
        events[event_id] = canonical
    return events, physical


if len(sys.argv) != 3:
    fail("usage: verify_replay.py BEFORE_SUMMARIES_JSONL AFTER_SUMMARIES_JSONL")

before, before_physical = logical_events(sys.argv[1])
after, after_physical = logical_events(sys.argv[2])
if len(before) != 2:
    fail("expected two logical initial summaries")
if after != before:
    fail("duplicate result changed or created a logical run summary revision")

print(
    "DEDUP      duplicate result publication created no logical summary/revision; "
    f"Kafka summary copies {before_physical}->{after_physical}"
)
