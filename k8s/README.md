# Kubernetes Manifests

These manifests separate the Pulse application from demo dependencies.

| Path | Purpose | Dependency model |
| --- | --- | --- |
| `base/` | Pulse service account, config, service, PDB, and deployment | external Kafka, Redis, target, and monitoring |
| `demo/` | deterministic gRPC target, single-node Kafka/Redis, Prometheus/Grafana | demo only; no HA or production durability |
| `overlays/kind/` | local/kind exercise | includes both `base` and `demo` |
| `overlays/staging/` | production-oriented application example | `base` only; managed dependencies required |
| `overlays/prod/` | production-oriented application example | `base` only; managed dependencies required |

The compatibility entrypoint `k8s/kustomization.yaml` renders the kind/demo overlay;
it is never a production default.

Render all manifests without contacting a cluster:

```bash
make k8s-validate
```

Deploy the complete kind stack from repository-owned images:

```bash
make k8s-deploy-kind
kubectl --context kind-account -n pulse-dev get deploy,pod
make k8s-pf-grafana
```

`k8s-deploy-kind` builds and loads both the Pulse demo runtime and
`pulse-demo-target`, then waits for the target, Kafka, Redis, Prometheus, Grafana, and
Pulse rollouts. It explicitly restarts the two local-image deployments after loading,
so reusing the `local` tags cannot leave old pods running. The kind scenario uses
`pulse.demo.v1.DemoService/Echo`; no external `account` service or descriptor is
required. Its healthy and deliberately failing requests repeat throughout soak/chaos
runs. The latter are target measurements, not automatic whole-slice retries.
Rollout waits are bounded by `K8S_ROLLOUT_TIMEOUT` (default `300s`).

The demo also creates `Service/pulse-metrics-headless`. Prometheus uses DNS service
discovery against that headless service so each ready Pulse replica is a distinct
scrape target; cluster totals and leader/follower panels are not sampled through a
random ClusterIP backend. This uses ordinary Kubernetes DNS and grants Prometheus no
Kubernetes API credentials.

The app-only base is intentionally not directly runnable: placeholder target and Kafka
endpoints must be patched, and Redis is read from
`/var/run/secrets/pulse/PULSE_REDIS_URL`. Staging/prod secret examples are templates;
do not commit real credentials. Topic management is disabled and topics should be
pre-created with environment-appropriate partitions, replication, retention, ACLs,
and `min.insync.replicas`. This includes the jobs, results, summaries, and DLQ topics;
the worker and aggregation consumer groups must remain distinct.
Results/DLQ retention must cover the execution-terminal retention window, summaries
must cover the aggregation retention window, and jobs retention must cover the
environment's longest supported recovery outage.

Replace `ghcr.io/your-org/pulse:0.2.0` with a reviewed immutable digest before an
environmental rollout. The example version is explicit to prevent an accidental
`latest` deployment, not evidence that the image exists in that registry.

## Security boundary

The container is non-root, drops capabilities, uses a read-only root filesystem, and
has explicit resource bounds. The current application configuration still lacks full
Kafka SASL/TLS and Redis certificate/auth configuration. Dynamic gRPC is plaintext
HTTP/2 only; this build rejects `https://` because tonic TLS transport, custom CA, and
mTLS are not enabled. Resolve these limitations before crossing an untrusted network.
The manifests are not a production architecture or support statement.

Only kind/demo commands manage `Deployment/grpc-target`, `Deployment/kafka`,
`Deployment/redis`, Prometheus, or Grafana. Staging/prod operators must use their
managed-service runbooks and monitoring.
