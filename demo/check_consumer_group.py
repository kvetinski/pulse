#!/usr/bin/env python3
"""Fail closed unless every partition for a Kafka consumer group is caught up."""

from __future__ import annotations

import sys
from pathlib import Path


def caught_up(snapshot: str, topic: str) -> bool:
    seen = False
    for line in snapshot.splitlines():
        fields = line.split()
        if len(fields) < 6 or fields[1] != topic:
            continue

        seen = True
        current_text, end_text = fields[3], fields[4]
        try:
            end = int(end_text)
        except ValueError:
            return False

        # Kafka renders an uninitialized current offset as "-". It is caught
        # up only when the partition has never contained a record.
        if current_text == "-":
            if end != 0:
                return False
            continue

        try:
            current = int(current_text)
        except ValueError:
            return False
        if current < end:
            return False

    return seen


def main() -> int:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} SNAPSHOT TOPIC", file=sys.stderr)
        return 2

    snapshot = Path(sys.argv[1]).read_text()
    return 0 if caught_up(snapshot, sys.argv[2]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
