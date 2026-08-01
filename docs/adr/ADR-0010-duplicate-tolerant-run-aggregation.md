# ADR-0010: Duplicate-Tolerant Run Aggregation

- Status: Accepted
- Date: 2026-07-31

## Context

Scenario windows are split across Kafka jobs. Per-slice p95 or p99 values cannot be
averaged into a mathematically valid run percentile, and an acknowledged result may be
published twice if the worker crashes before recording completion or committing its
source offset.

## Decision

Every window has a deterministic `run_id`, total slice count, and deterministic
execution identities. Results carry a deterministic terminal-event identity, counters,
and fixed mergeable latency buckets in the versioned Kafka contract.

The run aggregator:

1. consumes the results topic with its own consumer group and bounds the number of
   active runs, scan batches, error kinds, and pending outbox reads;
2. accepts results only when their contract and slice metadata validate;
3. atomically indexes accepted results by deterministic slice/execution identity in
   Redis before synchronously committing the result offset;
4. ignores duplicate and out-of-order delivery;
5. sums counts and histogram buckets rather than averaging quantiles;
6. emits `complete`, `partial`, `timed_out`, or `cancelled` summaries with explicit
   missing slice indexes; and
7. stores each revision in a Redis publication outbox, safely updates an incomplete
   summary if a late missing result arrives, and publishes the revision to the summary
   topic before acknowledging exactly that outbox revision; and
8. uses Redis `TIME` inside the atomic scripts for first-result timestamps, deadline
   scans, expiry decisions, finalization, and retention. An aggregator host clock that
   is fast or slow cannot finalize a run early or defer it indefinitely; and
9. bounds result ingest/commit and maintenance cycles below Kafka's max-poll interval.
   Exceeding that deadline fail-stops the aggregator with the source offset unsettled.

Automatic completion emits `complete`. Deadline expiry emits `timed_out`. `partial`
and `cancelled` are explicit finalization states supported by the store contract; a
cancelled run rejects unseen slices. A late slice may advance `partial` or `timed_out`
to `complete`, creating a new deterministic revision. A stale acknowledgement cannot
clear that newer revision.

Contract versions 1 through the current version are accepted when required fields are
valid. Unknown future versions fail closed so they can follow the poison-message DLQ
path rather than being misinterpreted.

Version 1 results do not contain a mergeable latency histogram. Their counts remain
compatible, but a version 1 aggregate has no authoritative global quantiles; zero is
the explicit unavailable value. Mixed schema versions within one run are rejected.

## Consequences

- Global quantiles are reproducible approximations at the published bucket resolution.
- Duplicate publication does not double counts or corrupt percentiles.
- Completeness is explicit; a partial result is never presented as a complete run.
- The long-running Kafka result consumer, Redis aggregation/deadline/outbox store, and
  Kafka summary publisher are wired into startup, readiness, and bounded shutdown.
- Result aggregation cannot silently occupy the consumer beyond its validated Kafka
  polling budget; an uncertain result is replayed and deduplicated after recovery.
- Kafka and Redis are not one transaction. A crash after summary publication but before
  outbox acknowledgement may republish the same deterministic `event_id` and revision;
  downstream consumers must deduplicate them.
- Redis retains aggregation state only for the configured TTL. Late results after that
  retention window cannot revise a discarded run.
