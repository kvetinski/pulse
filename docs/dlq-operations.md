# DLQ Operations

This document defines Dead-Letter Queue (DLQ) retention and replay procedures for Pulse.

## Retention Policy

Retention is enforced at the Kafka topic level (`PULSE_KAFKA_DLQ_TOPIC`).

Recommended retention by environment:
- dev: 7 days (`604800000` ms)
- staging: 14 days (`1209600000` ms)
- prod: 30 days (`2592000000` ms)

### Retention Configuration

Example (adjust topic name and cluster context):

```bash
# dev (7d)
/opt/kafka/bin/kafka-configs.sh --bootstrap-server kafka:9092 \
  --entity-type topics --entity-name pulse.scenario.dlq \
  --alter --add-config retention.ms=604800000

# staging (14d)
/opt/kafka/bin/kafka-configs.sh --bootstrap-server kafka:9092 \
  --entity-type topics --entity-name pulse.scenario.dlq \
  --alter --add-config retention.ms=1209600000

# prod (30d)
/opt/kafka/bin/kafka-configs.sh --bootstrap-server kafka:9092 \
  --entity-type topics --entity-name pulse.scenario.dlq \
  --alter --add-config retention.ms=2592000000
```

## Replay Safety Rules

- Replay is **idempotent-only**.
- The tool replays structured `FailedScenarioJob` records only. A recognized,
  valid poison-message envelope cannot be reconstructed safely, so it is reported
  and skipped. In execution mode its offset is synchronously committed for the
  dedicated replay group; the evidence remains in Kafka until topic retention.
- Unknown, malformed, ambiguous, invalid-base64, and unsupported/future-version
  envelopes stop replay with their offset unsettled for manual inspection.
- Poison `event_id` and source topic/partition/offset coordinates are the dedupe
  identity. Key and payload evidence may be deterministic prefixes with explicit
  original byte counts and truncation flags; it is not a complete forensic payload.
  Operators who require full raw bytes must retain or export the source Kafka topic.
- Fix the root cause before replaying (bad schema, endpoint outage, dependency outage).
- Replay in controlled batches at a reduced record-publication pace; each accepted V2
  job retains its stamped scenario load.
- Use a dedicated consumer group for replay to avoid disturbing other consumers.
- Set `PULSE_DLQ_REPLAY_SCALE=1.0` and a record rate below the approved envelope. V2
  rejects arbitrary load scaling because it would violate the stamped deterministic
  local plan; a future authenticated override contract requires its own ADR. The replay
  CLI still enforces global rate, concurrency, and duration ceilings before publishing.

## Decision Checklist (Before Replay)

- Scenario config unchanged, or drift is understood and explicitly accepted.
- Root cause is resolved.
- Replay record rate is set and reviewed; scale is exactly `1.0`.
- Idempotency is confirmed for all scenarios being replayed.

## Replay Workflow

1. Dry-run to inspect DLQ contents and filters.
2. Execute replay with reduced record pacing and explicit idempotency confirmation.
3. Monitor DLQ rate, scenario failure rate, p95 latency, and result publish failures.

Each accepted DLQ record becomes a new one-slice run with `attempt=0`. Its run and
execution identities are deterministically bound to the source DLQ topic, partition,
offset, and failed terminal-event identity. Its stable scheduled timestamp is the
failed record's durable failure time. Consequently, redelivery after replacement-job
publication succeeds but the synchronous source commit fails republishes the exact same
job identity; a different source offset cannot collide with it. This deliberately
executes target traffic again; it is not a metadata-only reprocessing operation. A
republish or commit failure stops before later offsets advance.

The replay consumer disables automatic commits and automatic offset storage. It uses
the configured max-poll interval, session timeout, socket/request timeout, prefetch,
and fetch byte limits. Startup fails when the maximum pacing interval plus one bounded
publish and synchronous commit can reach `PULSE_KAFKA_MAX_POLL_INTERVAL_MS`; operators
must raise the poll interval or increase replay record pacing rather than risk a silent
rebalance during settlement.

### Dry-Run

```bash
PULSE_DLQ_REPLAY_DRY_RUN=true \
PULSE_DLQ_REPLAY_SCENARIO_IDS=CreateGetDelete \
PULSE_DLQ_REPLAY_REASON_CONTAINS=timeout \
cargo run --bin pulse_dlq
```

### Execute Replay (Idempotent-Only)

```bash
PULSE_DLQ_REPLAY_DRY_RUN=false \
PULSE_DLQ_REPLAY_CONFIRM_IDEMPOTENT=true \
PULSE_DLQ_REPLAY_RATE_PER_SEC=5 \
PULSE_DLQ_REPLAY_SCALE=1.0 \
PULSE_DLQ_REPLAY_LIMIT=1000 \
cargo run --bin pulse_dlq
```

## Replay Environment Variables

- `PULSE_KAFKA_BROKERS`
- `PULSE_KAFKA_DLQ_TOPIC`
- `PULSE_KAFKA_JOBS_TOPIC`
- `PULSE_SCENARIOS_FILE` (optional; defaults to `./scenarios.yaml`)
- `PULSE_DLQ_REPLAY_GROUP_ID` (default: `pulse-dlq-replay`)
- `PULSE_DLQ_REPLAY_DRY_RUN` (default: `true`)
- `PULSE_DLQ_REPLAY_CONFIRM_IDEMPOTENT` (must be `true` to execute)
- `PULSE_DLQ_REPLAY_RATE_PER_SEC` (default: `5`)
- `PULSE_DLQ_REPLAY_SCALE` (must be `1.0`; pacing is controlled separately)
- `PULSE_DLQ_REPLAY_LIMIT` (records inspected; default: `1000`, range: `1..=10000`)
- `PULSE_DLQ_REPLAY_SCENARIO_IDS` (comma-separated)
- `PULSE_DLQ_REPLAY_REASON_CONTAINS` (substring)
- `PULSE_DLQ_REPLAY_SINCE_UNIX_MS`
- `PULSE_DLQ_REPLAY_UNTIL_UNIX_MS`

Dry-run consumes and reports matches but does not commit the replay consumer-group
offsets. In execution mode, filtered records are committed for the dedicated replay
group; they remain present in Kafka until topic retention and can be reviewed with a
different group. Each invocation scans a finite number of records. Reason summaries
retain at most 64 sanitized, 160-byte labels and aggregate additional labels under
`<other reasons>`.
