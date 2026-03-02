# Pulse

Distributed, scenario-driven load engine for gRPC services.

Pulse schedules scenario executions, distributes work through Kafka, coordinates leadership and idempotency with Redis, and executes chained gRPC calls with per-step metrics.

## What Pulse does

- Executes scenario chains (`step1 -> step2 -> ...`) with shared per-scenario context.
- Supports dynamic gRPC calls from descriptor sets (no generated typed client required).
- Supports per-step endpoint overrides (different services in one scenario).
- Controls scenario start rate (`scenarios_per_sec`) and in-flight concurrency (`max_concurrency`).
- Splits high-load scenarios into slices and distributes them across worker replicas.
- Publishes structured run results to Kafka and prints latency percentiles in CLI logs.

## Current status

Implemented:

- YAML scenario loader with schema validation (`version: 1`).
- Dynamic gRPC unary execution using compiled descriptor sets.
- Request templating (`${ctx.*}`, `${gen.*}`), response extraction, context reuse.
- Distributed scheduler/worker runtime with Redis leader election and Kafka job queue.
- Graceful shutdown handling (`SIGINT`/`SIGTERM`) across runtime loops.
- Worker-level retry with exponential backoff and Kafka dead-letter topic.
- Bounded Kafka producer/consumer client queues.
- Docker Compose and Kubernetes manifests.

Not implemented yet:

- HTTP step adapter (schema exists, runtime returns error).
- WebSocket step adapter.
- gRPC streaming methods (unary only).

## Architecture

Pulse is split by clean boundaries:

- `domain/`
  - Core contracts and abstractions (`Step`, `Scenario`, ports, errors).
- `application/`
  - Scenario parsing, template rendering, runner, scheduler/worker orchestration, metrics.
- `infrastructure/`
  - Kafka adapters (jobs/results), Redis adapters (leader/due/idempotency), dynamic gRPC gateway.

Runtime flow:

1. All replicas start scheduler + worker loops.
2. Redis leader lock elects one active scheduler.
3. Leader marks due scenarios in Redis and publishes sliced jobs to Kafka.
4. Worker group consumes jobs from Kafka.
5. Idempotency key in Redis prevents duplicate execution.
6. Scenario steps execute; step/scenario metrics are collected.
7. Result summary is published to Kafka results topic.

## Repository layout

- `src/main.rs`: bootstrap, config, gateway initialization.
- `src/application/service.rs`: leader/scheduler/worker runtime.
- `src/application/scenarios.rs`: YAML schema + validation + conversion to domain.
- `src/application/steps.rs`: dynamic gRPC step execution.
- `src/infrastructure/grpc/dynamic_gateway.rs`: descriptor-backed gRPC caller.
- `scenarios.yaml`: local/compose scenario file.
- `k8s/scenarios.k8s.yaml`: in-cluster scenario file.
- `k8s/kustomization.yaml`: Kubernetes assembly entrypoint.
- `k8s/dashboards/pulse-runtime-dashboard.json`: dashboard bundled for in-cluster Grafana.
- `k8s/examples/hpa-pulse.yaml`: sample HPA for pulse deployment.
- `k8s/examples/pdb-pulse.yaml`: sample stricter PDB (`minAvailable: 2`).
- `k8s/*.yaml`: Kubernetes manifests.
- `docs/architecture-decisions.md`: runtime architecture decisions and tradeoffs.
- `docs/benchmarks.md`: measured benchmark results (environment, throughput, latency, error, resource snapshot).
- `docs/operational-safety.md`: shutdown, retry, DLQ, and queue safety behavior.
- `docs/testing-plan.md`: test strategy.
- `docs/rollout-plan.md`: staged rollout plan.

## Prerequisites

- Rust toolchain (edition 2024; `cargo`).
- `protoc` installed locally.
- Docker + Docker Compose.
- (Optional) kind + kubectl for Kubernetes deployment.
- A target gRPC service (example scenarios use `account.v1.AccountService`).

## Build descriptor set

Pulse dynamic gRPC uses a compiled `FileDescriptorSet`.

Default:

```bash
make proto-descriptor
```

Output:

- `descriptors/services.pb`

Multiple services/protos:

```bash
make proto-descriptor \
  PROTO_FILES="src/account.proto ../payments/proto/payment.proto" \
  PROTO_SRC_DIRS="src ../payments/proto" \
  PROTO_IMPORT_DIRS="/usr/include ../payments/proto"
```

## Scenario configuration (YAML)

Scenario source is loaded from:

- `PULSE_SCENARIOS_FILE` if set.
- Otherwise `./scenarios.yaml`.

Key fields:

- `scenarios_per_sec`: scenario starts per second (not requests per second).
- `max_concurrency`: max in-flight scenario executions.
- `duration`: load window (`Ns`, `Nm`, `Nh`).
- `repeat`:
  - `type: once`
  - `type: every` + `interval: Ns|Nm|Nh`
- `partition_key_strategy`: `execution_key` (default) or `scenario_id`.

gRPC step fields:

- `protocol: grpc`
- `service`, `method`
- Optional step `endpoint` (overrides scenario endpoint)
- One request form:
  - `request_fields` (JSON object; encoded using descriptor schema)
  - `request_base64` (raw protobuf bytes in base64; can be templated)
- Optional `extract` map (`ctx_key: response.path`)
- Optional `response_payload_context_key` (stores raw response bytes as base64)

Example:

```yaml
version: 1
scenarios:
  - name: CreateGetDelete
    endpoint: http://host.docker.internal:8080
    scenarios_per_sec: 5
    max_concurrency: 20
    duration: 30s
    repeat:
      type: every
      interval: 1m
    steps:
      - protocol: grpc
        service: account.v1.AccountService
        method: CreateAccount
        request_fields:
          phone: "${gen.phone}"
        extract:
          user_id: "account.id"
      - protocol: grpc
        method: GetAccount
        service: account.v1.AccountService
        request_fields:
          id: "${ctx.user_id}"
      - protocol: grpc
        service: account.v1.AccountService
        method: DeleteAccount
        request_fields:
          id: "${ctx.user_id}"
```

### Template expressions

Supported placeholders:

- Context:
  - `${ctx.user_id}`
- Generators:
  - `${gen.uuid}`
  - `${gen.phone}`
  - `${gen.int:1:100}`

## Run locally (binary)

Start dependencies yourself (Kafka/Redis), then run:

```bash
make start
```

Common overrides:

```bash
PULSE_KAFKA_BROKERS=localhost:9092 \
PULSE_REDIS_URL=redis://127.0.0.1:6379 \
PULSE_SCENARIOS_FILE=./scenarios.yaml \
PULSE_GRPC_DESCRIPTOR_SET=./descriptors/services.pb \
make start
```

## Run with Docker Compose

Compose starts Kafka + Redis + Pulse:

```bash
make docker-up
make docker-logs
```

Notes:

- Compose uses `scenarios.yaml` mounted to `/app/scenarios.yaml`.
- Default target endpoint is `http://host.docker.internal:8080`.
- Ensure your target gRPC service is reachable from the host at that address.
- Compose also starts:
  - Prometheus: `http://localhost:19091`
  - Grafana: `http://localhost:13000` (default `admin/admin`)
- Grafana auto-provisions:
  - Prometheus datasource
  - `Pulse Runtime Metrics` dashboard from `ops/grafana/dashboards/pulse-runtime-dashboard.json`

Stop:

```bash
make docker-down
```

## Run on Kubernetes (kind)

Deploy with local image load:

```bash
make k8s-deploy-kind
```

Useful checks:

```bash
make k8s-status
make k8s-logs
make k8s-kafka-topics
```

Notes:

- Kubernetes uses `k8s/scenarios.k8s.yaml`.
- Pulse runs in namespace `pulse` (default context: `kind-account`).
- Example target endpoint is cross-namespace to Account service: `http://account.account:8080`.
- Deployment exposes `/metrics` on port `9090` through `Service/pulse`.
- Pulse workloads run as non-root and include startup/readiness/liveness probes.
- `make k8s-deploy` applies `k8s/kustomization.yaml` and provisions a dedicated observability stack in `pulse` namespace:
  - `Deployment/Service prometheus` using `k8s/prometheus.yaml`.
  - `Deployment/Service grafana` using `k8s/grafana.yaml`.
  - ConfigMap `pulse-runtime-dashboard` is generated by kustomize from `k8s/dashboards/pulse-runtime-dashboard.json` and mounted into Grafana.
- Optional hardening examples:
  - `make k8s-apply-hpa-example` to apply `k8s/examples/hpa-pulse.yaml` (requires metrics-server).
  - `make k8s-apply-pdb-example` to apply stricter disruption policy (`minAvailable: 2`).

Access examples:

```bash
kubectl --context kind-account -n pulse port-forward svc/prometheus 9090:9090
kubectl --context kind-account -n pulse port-forward svc/grafana 3001:3000
```

If metrics-server is not healthy on kind:

```bash
make k8s-fix-metrics-server
```

HPA on kind requires metrics-server. If `kubectl top` fails or HPA shows unknown metrics, run `make k8s-fix-metrics-server` first.

## Metrics and results

Per scenario run:

- Total/success/failure counts.
- Scenario latency p50/p95/p99.
- Per-step latency p50/p95/p99 and success/failure.
- Error breakdown by kind.

Outputs:

- CLI summary in worker logs.
- Structured result message to Kafka `PULSE_KAFKA_RESULTS_TOPIC`.

## Configuration reference

Primary environment variables:

- `PULSE_KAFKA_BROKERS` (default: `localhost:9092`)
- `PULSE_KAFKA_JOBS_TOPIC` (default: `pulse.scenario.jobs`)
- `PULSE_KAFKA_RESULTS_TOPIC` (default: `pulse.scenario.results`)
- `PULSE_KAFKA_DLQ_TOPIC` (default: `pulse.scenario.dlq`)
- `PULSE_KAFKA_GROUP_ID` (default: `pulse-workers`)
- `PULSE_REDIS_URL` (default: `redis://127.0.0.1:6379`)
- `PULSE_REDIS_LEADER_KEY` (default: `pulse:leader`)
- `PULSE_REDIS_SCHEDULE_PREFIX` (default: `pulse:schedule`)
- `PULSE_REDIS_IDEMPOTENCY_PREFIX` (default: `pulse:dedupe`)
- `PULSE_NODE_ID` (default: `node-<pid>`)
- `PULSE_LEADER_LOCK_TTL_MS` (default: `10000`)
- `PULSE_LEADER_RENEW_INTERVAL_MS` (default: `3000`)
- `PULSE_SCHEDULER_TICK_INTERVAL_MS` (default: `500`)
- `PULSE_QUEUE_CAPACITY` (default: `1024`)
- `PULSE_WORKER_MAX_RETRIES` (default: `2`)
- `PULSE_WORKER_RETRY_BASE_DELAY_MS` (default: `500`)
- `PULSE_ENDPOINT` (default: `http://127.0.0.1:8080`)
- `PULSE_SCENARIOS_FILE` (optional)
- `PULSE_GRPC_DESCRIPTOR_SET` (required for dynamic gRPC scenarios)
- `PULSE_METRICS_ENABLED` (default: `true`)
- `PULSE_METRICS_BIND` (default: `0.0.0.0:9090`)

Prometheus scrape endpoint:

- `GET /metrics` on `PULSE_METRICS_BIND`
- Docker Compose host mapping: `http://localhost:19090/metrics`

Grafana sample dashboard:

- `ops/grafana/dashboards/pulse-runtime-dashboard.json`

## Development

```bash
make fmt
make check
cargo test
make bench
make ci-check
```

Docker-backed integration tests (Redis + Kafka):

```bash
make test-integration-compose
```

Optional overrides:

```bash
make test-integration-compose \
  TEST_KAFKA_BROKERS=127.0.0.1:19092 \
  TEST_REDIS_URL=redis://127.0.0.1:16379
```

## Troubleshooting

- `invalid yaml: unknown field ...`:
  - Container likely running stale binary/image; rebuild and recreate.
- `dynamic gRPC gateway is not configured for endpoint ...`:
  - Step endpoint exists in YAML but gateway was not initialized (check descriptor + endpoint config).
- `connect failed: transport error`:
  - Endpoint not reachable from runtime environment (host vs container vs cluster DNS mismatch).
- `metrics-server 0/1` in kind:
  - Run `make k8s-fix-metrics-server`.

## Roadmap

Short-term:

- Implement HTTP step adapter.
- Add per-step retry policies (currently worker-level retry).
- Add DLQ replay tooling.
- Add k8s base/overlay split for environment-specific deploys.

Medium-term:

- Implement WebSocket step adapter.
- Add gRPC streaming support (client/server/bidi).

## License

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
