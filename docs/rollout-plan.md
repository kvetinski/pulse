# Distributed Pulse Rollout Plan

Pulse is production-oriented but experimental. The staging/prod overlays are app-only
examples and are not approval to expose a target to load.

## Gate 0: Offline Review

- Run `make release-validate`, `make ci`, and `make k8s-validate`.
- Run `make validate-config` and review every deterministic slice, rate, concurrency
  allocation, target host, and duration.
- Confirm the target host is allowlisted and obtain explicit owner approval for the
  load window.
- Confirm target-side idempotency for any externally visible side effects.
- Pre-create jobs/results/DLQ topics with reviewed partitions, replication, retention,
  ACLs, and `min.insync.replicas`; keep Pulse topic management disabled.
- Provision externally managed Redis and Kafka. The kind/demo single-node resources
  are not a production dependency topology.
- Resolve the documented Kafka/Redis TLS and authentication limitations before using
  an untrusted network. This build rejects `https://` gRPC targets and its plaintext
  target transport must remain on a trusted network.

## Gate 1: Local Failure Semantics

- Run `make demo` and verify the deterministic result assertion, health endpoints, and
  metrics.
- Run unit, paused-time, contract-compatibility, and Redis coordination tests.
- Run Compose integration tests when Docker is available.
- Record any unavailable check and its exact environmental reason.

## Gate 2: One-Replica Staging Canary

- Deploy an immutable image digest with one Pulse replica and topic management off.
- Use a low-rate, short-duration, side-effect-safe canary.
- Verify readiness requires Redis, Kafka, initialized scenarios, and worker acceptance.
- Verify each source offset is committed only after an acknowledged terminal output.
- Compare expected and accepted slice identities; investigate missing or duplicate
  records even when aggregate counts look plausible.

## Gate 3: Failure Injection

- Inject Redis unavailability during claim and renewal; confirm no duplicate/success is
  inferred and no unsettled source offset is committed.
- Inject result and DLQ publication failure; confirm the source remains unsettled.
- Kill a worker after lease acquisition and after output acknowledgement; confirm lease
  expiry/recovery and duplicate-tolerant aggregation.
- Interrupt scheduler publication and leadership; confirm only missing deterministic
  slices are resumed.
- Terminate a worker during execution and publication; confirm bounded drain and honest
  incomplete-work reporting.
- A chaos run passes only when data-plane counters/results recover, not merely when pods
  roll out successfully.

## Gate 4: Multi-Replica Staging

- Scale gradually while preserving one fenced scheduler leader.
- Validate Kafka group behavior and `max.poll.interval.ms` against the configured
  execution/settlement budget.
- Watch consumer lag, oldest job age, lease recovery/conflicts, publication failures,
  commit failures, incomplete dispatches, and partial run summaries.
- Retain the evidence bundle defined in `docs/evidence-policy.md`.

## Gate 5: Production-Oriented Review

- Require an explicit target owner, rate/duration envelope, rollback condition, and
  change window.
- Review managed dependency HA, backups, capacity, TLS/auth, ACLs, and topic retention.
- Replace version placeholders with a reviewed digest.
- Keep `PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS=false`; use a narrow allowlist.
- Roll out one replica and a minimal canary before raising replicas or load.
- Stop scheduling first on rollback; do not manually skip unsettled Kafka offsets.

## Rollback

1. Mark Pulse unready and stop new scheduling.
2. Allow the bounded shutdown drain to publish terminal dispositions.
3. Roll back the immutable image/config together.
4. Verify Redis lease/dispatch recovery and Kafka lag before resuming load.
5. Record duplicate possibility and missing-slice state in the incident evidence.
