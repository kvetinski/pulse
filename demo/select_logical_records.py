#!/usr/bin/env python3
"""Select deterministic logical Kafka records and validate physical duplicates."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any


def fail(message: str) -> None:
    print(f"demo logical-record selection failed: {message}", file=sys.stderr)
    raise SystemExit(2)


def main() -> None:
    if len(sys.argv) != 6:
        fail(
            "usage: select_logical_records.py "
            "INPUT_JSONL OUTPUT_JSONL EXPECTED_UNIQUE ID_FIELD RECORD_NAME"
        )
    source, destination, expected_raw, identity_field, record_name = sys.argv[1:]
    try:
        expected = int(expected_raw)
    except ValueError:
        fail(f"expected unique count is not an integer: {expected_raw!r}")
    if expected <= 0:
        fail("expected unique count must be positive")

    physical = 0
    unique: dict[str, tuple[str, dict[str, Any]]] = {}
    for line_number, raw in enumerate(Path(source).read_text().splitlines(), start=1):
        if not raw.strip():
            continue
        physical += 1
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as error:
            fail(f"{source}:{line_number} is not JSON: {error}")
        if not isinstance(value, dict):
            fail(f"{source}:{line_number} is not a JSON object")
        identity = value.get(identity_field)
        if not isinstance(identity, str) or not identity:
            fail(
                f"{source}:{line_number} has no deterministic "
                f"{identity_field}"
            )
        canonical = json.dumps(value, sort_keys=True, separators=(",", ":"))
        if identity in unique and unique[identity][0] != canonical:
            fail(
                f"{identity_field} {identity!r} was reused for different "
                f"{record_name} payloads"
            )
        unique.setdefault(identity, (canonical, value))

    if len(unique) < expected:
        raise SystemExit(1)
    if len(unique) > expected:
        fail(f"expected {expected} logical {record_name}, found {len(unique)}")

    ordered = [unique[identity][1] for identity in sorted(unique)]
    Path(destination).write_text("".join(json.dumps(value) + "\n" for value in ordered))
    duplicates = physical - len(unique)
    print(
        f"{record_name.upper():<10} Kafka records={physical}, "
        f"logical {record_name}={len(unique)}, identical copies={duplicates}"
    )


if __name__ == "__main__":
    main()
