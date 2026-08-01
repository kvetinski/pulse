# Pulse

Failure-aware distributed gRPC scenario engine in Rust, built around Tokio, Kafka, and
Redis.

Pulse turns a versioned YAML scenario into deterministic load slices, dispatches them
through Kafka, coordinates fenced scheduling and renewable execution leases in Redis,
executes dynamic unary gRPC calls from descriptor sets, and publishes durable per-slice
measurements. The interesting part is not another protocol adapter: it is what happens
at every Kafka/Redis/target crash boundary.

**Status:** experimental and production-oriented, with explicit limitations. Pulse is
not described as production-grade.

**Delivery guarantee:**

> At-least-once target execution with deterministic job identities, lease-based
> duplicate suppression, durable terminal-event publication, and duplicate-tolerant
> result aggregation.

Pulse does **not** claim exactly-once target execution. An external gRPC side effect
cannot be atomically committed with Kafka and Redis, a crash at that boundary can call
the target again. Side-effecting targets need their own idempotency key or an explicit
operator decision that replay is safe.

## Five-minute local demo

The demo is self-contained. One command quietly prepares the containers, then draws a
two-replica topology and renders a chronological timestamped event ledger from the real
Pulse and target logs. It shows which node became the fenced scheduler, which node consumed each
Kafka partition/offset, where the gRPC failure occurred, when it occurred, and how it
became measurement data without a load retry. Kafka contracts and per-node metrics then
verify durable settlement, mergeable aggregation, and duplicate-result publication. It
does not contact an external target or require the reviewer to operate Kafka and Redis.

```bash
make demo
```

Cold image downloads can take a few minutes, but their output is captured under
`artifacts/demo/` instead of obscuring the system story. On success, the terminal shows
a compact evidence timeline like:

```text
TOPOLOGY   two eligible Pulse replicas, Redis elects exactly one scheduler

  pulse-demo-a  <== jobs/results ==> KAFKA <== jobs/results ==> pulse-demo-b
       +<== leader fence / ledger / leases / aggregate ==> REDIS <==+
        \--------------- unary gRPC --> TARGET <------------------/

05:31:00.484Z  +08.292s  TARGET ERR grpc-target   DemoService/Echo returned UNAVAILABLE
05:31:00.485Z  +08.293s  MEASURE   pulse-demo-b  classified target_status:Unavailable
FAILURE    05:31:00.484Z..05:31:01.987Z at grpc-target / DemoService/Echo
JOBS       Kafka records=3, logical jobs=3, identical copies=0
RESULTS    Kafka records=3, logical results=3, identical copies=0
SUMMARIES  Kafka records=..., logical summaries=2, identical copies=...
SETTLE     logical jobs=3, physical deliveries=..., source commits=..., uncommitted=0
POLICY     target failures caused automatic retries=0 and DLQ records=0
RECOVERY   aggregate duplicate counter 0 -> 1 [ok]
DEDUP      duplicate result publication created no logical summary/revision
PASS       real target traffic, durable settlement and failure semantics verified
```

The stack remains available for inspection:

- Pulse A readiness/metrics: <http://127.0.0.1:29090/health/ready>,
  <http://127.0.0.1:29090/metrics>
- Pulse B readiness/metrics: <http://127.0.0.1:29093/health/ready>,
  <http://127.0.0.1:29093/metrics>
- Prometheus: <http://127.0.0.1:29091>
- gRPC fixture: `127.0.0.1:25051` (`pulse.demo.v1.DemoService/Echo`)
- Kafka: `127.0.0.1:29092`
- Redis: `127.0.0.1:26379`

The machine-readable proof is retained under `artifacts/demo/`, including the raw
timestamped runtime log, parsed event ledger, logical and physical Kafka
job/result/summary contracts, both readiness snapshots, aggregator-group offsets,
duplicate-injection counters, pre/post-injection summary snapshots, and separate per-node Prometheus
snapshots. Worker placement is
reported as observed log evidence, it is intentionally not claimed as a field in the
version 2 Kafka result contract.

```bash
make demo-down
```

Demo prerequisites are Docker with Compose, Python 3, and `curl`, Rust and `protoc` run
inside the image build. Contributors can run `make doctor` to verify the pinned Rust
1.88.0 toolchain (including rust-analyzer) and local `protoc` as well.
The first image build can take a few minutes. `make demo` uses the fixed
`pulse-demo` Compose project and clears only that demo's volumes before the run. Before
creating either Compose network, Pulse deterministically selects an unused RFC1918 `/24`
that does not overlap existing Docker networks or host routes, and reuses the assigned
subnet while the project is running. This also works when Docker's default address pool
is exhausted. Set `PULSE_DOCKER_SUBNET` explicitly to override the selection.

## Why the failure model matters

- Redis coordination APIs return typed acquired/busy/completed/follower/error outcomes,
  dependency failure is never interpreted as duplicate, not-due, or success.
- Execution claims, renewals, completion, and release are atomic owner-checked Lua
  operations with lease-expiry recovery. Local validity is anchored before each Redis
  request, so slow responses consume rather than extend the lease budget.
- Scheduler windows and slices have deterministic identities, a Redis dispatch ledger
  advances only after every Kafka publication is acknowledged.
- Leadership has an opaque owner token and monotonic fence, stale leaders cannot mutate
  dispatch state after failover.
- Source offsets use broker-acknowledged synchronous commits only after a durable
  terminal disposition.
- Target gRPC status, transport failure, and deadline are measurements, not automatic
  whole-slice retries.
- Classified Pulse infrastructure failures publish attempt N+1 to the Kafka jobs topic
  with a deterministic identity and persisted `not_before_unix_ms`, attempt N is
  committable only after that retry and its Redis terminal state are acknowledged.
- Kafka polling is separated from settlement backoff by a bounded handoff queue, task
  sets are continuously reaped and shutdown drain is finite.
- Result aggregation and outbox maintenance have the same fail-stop Kafka polling
  envelope, leaving an uncertain result offset unsettled for duplicate-safe recovery.
- Fractional rates, including `0.1` scenarios/second, use monotonic time. Cross-slice
  concurrency uses quotient/remainder and never exceeds the configured global value.
- Per-slice results carry mergeable latency buckets. Duplicate/out-of-order aggregation
  durably merges them in Redis, ignores repeated deterministic execution identities,
  and publishes revisioned run summaries through a recoverable Redis outbox instead of
  averaging quantiles.

The normative invariants and crash-boundary diagram are in
[`ADR-0007`](docs/adr/ADR-0007-failure-model-and-delivery-semantics.md).

## Job lifecycle

```text
fenced scheduler
  -> prepare/resume deterministic Redis dispatch window
  -> publish only unacknowledged Kafka slices
  -> acknowledge each slice in the owner-checked ledger

Kafka source record
  -> validate contract (poison/permanent failure -> acknowledged DLQ)
  -> acquire renewable Redis execution lease
  -> run bounded scenario traffic
  -> publish deterministic result
  -> record owner-checked terminal outcome
  -> synchronously commit source offset

Kafka result record
  -> validate contract (poison/permanent failure -> acknowledged DLQ)
  -> atomically deduplicate and merge counts/histogram buckets in Redis
  -> synchronously commit result offset after durable acceptance
  -> publish complete/timed-out/late-complete summary revision from Redis outbox
  -> acknowledge exactly that outbox revision
```

Kafka publication and Redis acknowledgement are not one transaction. If Kafka accepts
an output and the process dies before Redis completion or source commit, redelivery can
republish the same deterministic output. That ambiguity prefers duplicates over loss,
the aggregator is duplicate tolerant.

## Failure semantics

| Failure point                                  | Recovery behavior                                                  |                         Offset committed? |                Duplicate possible? |
| ---------------------------------------------- | ------------------------------------------------------------------ | ----------------------------------------: | ---------------------------------: |
| Redis unavailable during claim                 | fail-stop, Kafka recovers unsettled source                         |                                        No |                 no target call yet |
| Execution lease already held                   | retain record in place, recheck terminal/expiry within poll budget | only after terminal verification/recovery |             no while lease is live |
| Crash after lease acquisition                  | lease expires, redelivery recovers                                 |                                        No |                                Yes |
| Stale worker resumes                           | owner check rejects renew/complete/release                         |                                        No |        recovery worker may execute |
| Target returns non-OK                          | publish failed measurement, do not replay slice                    |                      After durable result |                 no automatic retry |
| Request deadline                               | publish timeout measurement                                        |                      After durable result |                 no automatic retry |
| Result publication fails                       | bounded retry, then retain unsettled source                        |                                        No |              Yes on later recovery |
| DLQ publication fails                          | retain poison/permanent source                                     |                                        No |                 DLQ may be retried |
| Output acked, Redis completion fails           | redelivery may republish same event ID                             |                                        No |        Yes, ignored by aggregation |
| Redis completion succeeds, source commit fails | verify durable terminal state on redelivery                        |                        no proof of commit |             no target re-execution |
| Partial scheduler publication                  | resume only ledger-missing deterministic slices                    |                                       n/a |         ambiguous slice may repeat |
| Leadership lost during dispatch                | local stop plus Redis fence rejection                              |                                       n/a | next leader resumes missing slices |
| Shutdown during execution                      | stop intake, drain to deadline, leave ambiguity unsettled          |                        terminal work only |                                Yes |

## Implemented runtime

- Rust 2024/Tokio runtime with bounded launch and consumer queues.
- Scenario chains with shared context, generated/template values, and response
  extraction.
- Dynamic unary gRPC from protobuf descriptor sets, service/method startup validation,
  connect timeout, per-step deadline, and whole-scenario deadline.
- Fractional token-bucket pacing with explicit optional startup burst.
- Deterministic Kafka contracts (current version 2) for jobs, results, summaries,
  failures, and poison payloads.
- Configurable Kafka poll/session/prefetch, producer acknowledgements/idempotence,
  timeouts, partition/replication settings, and opt-in topic management.
- Redis `TIME`-based leader leases, monotonic fencing, recoverable dispatch ledgers,
  renewable execution leases, retained terminal outcomes, and durable run aggregation.
- A dedicated result consumer merges deterministic slices, persists deadlines and a
  bounded summary outbox, and publishes revisioned `ScenarioRunSummaryEvent` records.
- Explicit `ResultPublished`, `RetryPublished`, `DeadLetterPublished`,
  `DuplicateCompleted`, plus non-terminal `ExecutionLeaseBusy` and `RetryLater`
  dispositions. Busy rebalance redeliveries stay at the processor head until Redis
  exposes completion or lease recovery, dependency ambiguity still fails closed.
- Separate `/health/live`, `/health/ready`, and `/metrics` routes.
- Prometheus metrics, Grafana assets, Kubernetes application/demo separation, CI,
  supply-chain checks, ADRs, runbooks, and evidence policy.

## Dry-run and scenario shape

Validate the repository scenario and print the exact slice rate/concurrency plan without
starting Kafka/Redis or generating target traffic:

```bash
make validate-config
```

The same mode is available directly with `PULSE_DRY_RUN=true`. Non-local targets must
be exactly allowlisted with `PULSE_TARGET_ALLOWLIST`, the broad
`PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS=true` escape hatch must be a conscious operator
choice.

A scenario-start rate is not raw request rate: each scenario can contain several unary
steps.

```yaml
version: 1
scenarios:
  - name: LocalUnaryDemo
    endpoint: http://grpc-target:50051
    scenarios_per_sec: 0.5
    max_concurrency: 2
    duration: 10s
    repeat:
      type: once
    partition_key_strategy: execution_key
    steps:
      - protocol: grpc
        service: pulse.demo.v1.DemoService
        method: Echo
        request_fields:
          message: "${gen.uuid}"
        extract:
          echoed_message: message
```

Supported expressions include `${ctx.key}`, `${gen.uuid}`, `${gen.phone}`, and
`${gen.int:1:100}`. See [`demo/scenarios.yaml`](demo/scenarios.yaml) for the executable
fixture and [`docs/configuration.md`](docs/configuration.md) for all environment
variables and validation relationships.

## Health, shutdown, and operations

Readiness requires parsed configuration, reachable Redis, Kafka producers and consumer,
initialized scenarios, and an accepting worker. It becomes false as soon as shutdown
drain begins. Liveness remains process/event-loop oriented so a dependency outage does
not cause a restart loop by itself.

Shutdown becomes unready, stops leadership/scheduling/intake, keeps bounded work alive
for terminal settlement, drains to the configured deadline, relinquishes owned leases
where safe, and leaves ambiguous records uncommitted for redelivery. Kubernetes
termination grace exceeds that application deadline.

Operational procedures:

- [`docs/operational-safety.md`](docs/operational-safety.md)
- [`docs/runbook.md`](docs/runbook.md)
- [`docs/dlq-operations.md`](docs/dlq-operations.md)
- [`docs/slo-alerts.md`](docs/slo-alerts.md)

## Kubernetes deployment boundary

- `k8s/base`: Pulse application resources only.
- `k8s/demo`: deterministic gRPC fixture plus explicit single-node
  Kafka/Redis/Prometheus/Grafana resources.
- `k8s/overlays/kind`: self-contained local exercise, includes application, target,
  and demo dependencies with recurring healthy and expected-failure traffic.
- `k8s/overlays/staging` and `prod`: app-only examples requiring externally managed
  dependencies and monitoring.

```bash
make k8s-validate
```

Production-oriented overlays disable topic management, require a Redis secret file,
and use an explicit image version placeholder. Replace it with a reviewed digest. The
single-node demo resources are not a production Kafka/Redis architecture. See
[`k8s/README.md`](k8s/README.md).

## Verification

```bash
make ci
make test-integration-compose   # requires a working Docker daemon
make supply-chain-check         # builds/scans both images and writes SBOM/scan artifacts
make release-validate
```

`make ci` runs formatting, Clippy with warnings denied, all Rust targets/features,
deterministic Tokio and contract tests, the runtime smoke benchmark, Compose syntax,
descriptor generation, release consistency, and every Kubernetes render. Compose-backed
Kafka/Redis tests remain separate because they start services.

CI throughput thresholds are smoke checks on noisy runners, not capacity evidence.
Historical measurements are retained without inventing improved numbers in
[`docs/benchmarks.md`](docs/benchmarks.md), new claims must follow
[`docs/evidence-policy.md`](docs/evidence-policy.md).

## Verified limitations

- Exactly-once target execution is impossible in the current architecture, duplicate
  external side effects remain possible at cross-system crash boundaries.
- Every leased terminal send attempt is Redis owner-checked immediately before Kafka
  publication, but that check and broker acknowledgement are not atomic. A process
  pause in the remaining interval can publish a deterministic stale output, Redis
  prevents its source commit, aggregation deduplicates results, and an orphan retry can
  add target traffic. Eliminating this requires a fenced execution outbox.
- Summary publication is at least once: a crash after Kafka acknowledges a summary but
  before Redis acknowledges its outbox revision may republish that deterministic
  `event_id`/revision. Summary consumers must deduplicate those fields.
- There is no separate delayed-retry topic or broker-side delay. Retry intent and its
  `not_before_unix_ms` are durable in the normal Kafka jobs topic, while bounded local
  deferral can head-of-line block one processor (the independent bounded consumer pump
  continues polling until its handoff queue is full).
- Dynamic gRPC supports plaintext unary HTTP/2 targets (`http://`) only. This build
  rejects `https://` at startup because tonic TLS transport is not enabled. HTTP,
  WebSocket, and gRPC streaming are intentionally out of scope while failure semantics
  are being hardened.
- Kafka SASL/TLS configuration and full Redis TLS certificate/auth support are not yet
  exposed. Redis uses a single configured endpoint rather than native Cluster/Sentinel
  discovery, though multi-key Lua keys are hash-tag compatible. Dynamic gRPC has no
  TLS, custom-CA, or mTLS configuration path, use it only on a trusted network.
- Retry age is exported without identity labels as
  `pulse_worker_retry_job_age_seconds`, aggregate state remains bounded outcome counters
  rather than a per-run gauge. Use Kafka lag and the durable Redis outbox for a specific
  run, never add unbounded `run_id` labels.
- Kafka assignment epochs fence buffered commits across a real Compose-broker
  rebalance, and the test proves a fresh group member receives and commits the
  redelivery. Full-runtime long-scenario rebalances, managed Kafka variants, and
  prolonged rebalance/chaos runs remain environment-specific evidence requirements.
- Readiness does not yet expose per-scenario active-window fingerprint conflicts. A
  changed plan is rejected rather than mixed into an existing window, operators must
  restore the prior plan to finish that window using the runbook procedure.
- The kind/Compose brokers and Redis are deterministic demo fixtures, not HA or durable
  production dependencies.

## Documentation map

- [Architecture Decision Records](docs/adr/README.md): delivery, leases, fencing,
  dispatch recovery, retries, aggregation, rate distribution, and shutdown.
- [Reliability testing](docs/reliability-testing.md): deterministic/fault test matrix
  and data-plane chaos criteria.
- [Compatibility policy](docs/compatibility.md): scenario/Kafka/config/metric surfaces.
- [Rollout plan](docs/rollout-plan.md): fail-closed staged review gates.
- [Evidence policy](docs/evidence-policy.md): required raw data and claim levels.

## Release consistency

The crate/lockfile and deployment examples identify the next release as `0.2.0`,
existing tags `v0.1.0` through `v0.1.2` are recorded in the changelog. Rust 1.88.0 is
pinned for local tooling, rust-analyzer, CI, and Docker.
Before a future tag, update the crate, lockfile, and changelog together:

```bash
make release-validate VERSION=0.2.0
make release-tag VERSION=0.2.0
```

## License

[MIT](LICENSE)
