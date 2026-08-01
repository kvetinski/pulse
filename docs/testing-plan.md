# Distributed Pulse Testing Plan

The test strategy follows crash boundaries, not just modules. Prefer deterministic
fakes and paused Tokio time; use real Redis for Lua ownership/expiry behavior and real
Kafka only where broker acknowledgement, commits, rebalances, or partition ordering are
the behavior under test.

## Fast deterministic gates

- strict environment parsing and timeout/safety relationships;
- supported/unknown contract versions and malformed required metadata;
- deterministic run/window/slice/terminal identities;
- fractional rate pacing with paused time and explicit startup burst;
- cross-slice rate/concurrency sums and bounded `JoinSet` reaping;
- target failures as result measurements versus retryable Pulse failures;
- disposition-based commit authorization;
- duplicate/out-of-order histogram aggregation and missing-slice timeout;
- liveness/readiness/draining state transitions.

Run with:

```bash
cargo test --locked --all-targets --all-features
```

## Required failure matrix

| Boundary | Required assertion | Preferred environment |
| --- | --- | --- |
| Redis unavailable during execution claim | not duplicate; no source commit | fake + Compose |
| crash after lease acquisition | expiry permits recovery | real Redis |
| stale worker renew/complete | owner check rejects | real Redis |
| result publication failure | source stays uncommitted | deterministic fake |
| retry publication failure | no commit unless another terminal output succeeds | deterministic fake |
| DLQ publication failure | source stays uncommitted | deterministic fake |
| output ack then pre-commit crash | duplicate does not corrupt aggregation | deterministic fake + Kafka |
| invalid Kafka payload | deterministic poison DLQ; commit only after ack | fake + Kafka |
| partial scheduler publication | only missing slices republish | real Redis + fake publisher |
| leadership loss during dispatch | stale publisher stops/fence rejects | real Redis |
| target gRPC non-OK | failed result; no whole-slice retry | local fixture |
| Pulse dependency failure | bounded classified settlement retry | paused time |
| rate below one SPS | accurate inter-arrival timing | paused time |
| sliced load | exact global rate and concurrency sums | unit |
| long launch window | completed tasks reaped; bounded peak | paused time |
| slow gRPC call | step/scenario deadline and cancellation | local fixture |
| duplicate/out-of-order results | one correct aggregate | unit |
| missing result | partial/timed-out summary with missing indexes | paused time |
| shutdown during work/backoff/publication | finite drain; ambiguity uncommitted | deterministic fake |
| Kafka rebalance with a buffered source record | old assignment epoch cannot commit; redelivery recovers | Compose broker + Kafka adapter |

No unit fake can prove broker durability. Compose-backed tests are required before a
release claim:

```bash
make test-integration-compose
```

The Compose suite creates two members in one group, buffers a source record, forces a
rebalance, proves the old assignment epoch cannot commit it, and then proves a fresh
member receives and synchronously commits the redelivery.

## Reviewer demo

`make demo` is a short deterministic end-to-end assertion against the repository-owned
unary fixture. It verifies a consumed Kafka result, not merely healthy containers.

## Soak and chaos

```bash
make k8s-soak-chaos K8S_OVERLAY=kind SOAK_DURATION_SEC=1800
make k8s-check-performance K8S_OVERLAY=kind PERF_WINDOW=30m
```

The kind chaos script fails if jobs, durable results, and source commits do not make
data-plane progress. The overlay owns its recurring scenario, matching descriptor, and
deterministic `grpc-target`, so it requires no external account service. A
release-quality claim additionally needs the unique identity, completeness, raw-query,
and failure-timeline evidence in `docs/evidence-policy.md`.
