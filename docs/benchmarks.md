# Benchmarks

Last updated: 2026-03-02 12:46:08 UTC

## Environment

- Host OS: Linux 6.8.0-101-generic x86_64
- CPU: AMD Ryzen 7 5800H (16 vCPU reported by `nproc`)
- Rust toolchain: `cargo 1.92.0`, `rustc 1.92.0`
- Docker: `20.10.14`
- Kubernetes context: `kind-account`
- Kubernetes versions:
  - client: `v1.23.5`
  - server: `v1.29.2`
- Pulse deployment mode for gRPC scenarios: Kubernetes (`k8s/scenarios.k8s.yaml`)

## Method

- Source of truth for scenario throughput/latency/error:
  - Prometheus queries over a 1h window.
- Source of truth for Pulse resource usage:
  - `kubectl top pods -n pulse -l app=pulse` snapshot.
- Source of truth for runtime micro-benchmark:
  - `cargo run --release --bin pulse_bench --offline`.

## Scenario Results

Notes:
- `QPS` below is scenario executions/sec (not raw request/sec).
- Error rate is derived from `success` and `failure` scenario counters.
- Latencies are scenario end-to-end (seconds converted to ms).

| Scenario | Profile | QPS (observed) | p50 ms | p95 ms | p99 ms | Error rate | Pulse CPU/Memory |
|---|---|---:|---:|---:|---:|---:|---|
| DynamicGrpcGetStaticBase64 | `20 sps`, `20` concurrency, 3-step create/get/delete | 2.85 | 3.33 | 18.96 | 81.49 | 0.00% | 3-9m CPU, 4-12Mi mem (3-pod snapshot) |
| DynamicGrpcCreateGet | `3 sps`, `10` concurrency, once | 0.015 | 3.00 | 4.80 | 4.96 | 0.00% | 3-9m CPU, 4-12Mi mem (3-pod snapshot) |
| RunnerNoop (internal) | `pulse_bench` runner mode, 20 runs | 390.62 started/s | n/a | n/a | n/a | 0.00% | local process benchmark (not k8s load) |

Prometheus values used:
- `DynamicGrpcGetStaticBase64`
  - `qps_success_1h=2.8523870281803543`
  - `p50_s_1h=0.0033279845225901903`
  - `p95_s_1h=0.01895741324921136`
  - `p99_s_1h=0.08149494949494959`
  - `total_success_1h=10272.254628985509`
- `DynamicGrpcCreateGet`
  - `qps_success_1h=0.015051664285714281`
  - `p50_s_1h=0.003`
  - `p95_s_1h=0.0048000000000000004`
  - `p99_s_1h=0.00496`
  - `total_success_1h=54.203618571428564`

## Regression Thresholds

### CI-Enforced Smoke Thresholds (`pulse_bench`)

These are enforced in `make ci-check` and fail CI on regression.

| Metric | Threshold | Source |
|---|---:|---|
| Throughput floor | `runner_noop started_per_sec >= 120` | `pulse_bench` output |
| Latency ceiling | `runner_noop avg_run_ms <= 200` | `runner_elapsed / runs` in `pulse_bench` |
| Error-rate ceiling | `runner_noop drop_ratio <= 0.0` | `1 - finished/started` in `pulse_bench` |

Runtime env vars for overrides:
- `PULSE_BENCH_MIN_STARTED_PER_SEC` (default `120`)
- `PULSE_BENCH_MAX_AVG_RUN_MS` (default `200`)
- `PULSE_BENCH_MAX_DROP_RATIO` (default `0`)

### Runtime SLO Guardrails (Prometheus, manual gate for now)

These are documented thresholds for real gRPC scenario runs in k8s; not yet auto-gated in CI.

| Scenario | Throughput floor | p95 max | p99 max | Error rate max |
|---|---:|---:|---:|---:|
| DynamicGrpcGetStaticBase64 | `>= 2.0` scenario/s | `<= 30 ms` | `<= 120 ms` | `<= 0.5%` |
| DynamicGrpcCreateGet | `>= 0.01` scenario/s | `<= 10 ms` | `<= 25 ms` | `<= 0.5%` |

## Before/After Optimization Delta

Optimization: run Pulse runtime workload in `--release` profile for benchmark runs.

Benchmark command (same workload both runs):
- `PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=1000 PULSE_BENCH_RUNNER_ITERATIONS=20 cargo run --bin pulse_bench --offline`
- `PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=1000 PULSE_BENCH_RUNNER_ITERATIONS=20 cargo run --release --bin pulse_bench --offline`

Result (`runner_noop` throughput):
- Before (`dev`): `380.08 started/s`
- After (`release`): `390.62 started/s`
- Delta: `+2.77%`

## Reproduce

Runtime benchmark:

```bash
PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=1000 \
PULSE_BENCH_RUNNER_ITERATIONS=20 \
cargo run --release --bin pulse_bench --offline
```

Kubernetes metrics snapshot:

```bash
kubectl --context kind-account -n pulse top pods -l app=pulse
kubectl --context kind-account -n pulse exec deploy/prometheus -- \
  sh -lc "wget -qO- 'http://127.0.0.1:9090/api/v1/query?query=sum%20by%20(scenario%2Cstatus)%20(rate(pulse_scenario_executions_total%5B1h%5D))'"
```
