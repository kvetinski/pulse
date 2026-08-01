# Runtime Configuration

Pulse parses environment values strictly. A malformed boolean, integer, duration,
endpoint, broker, topic, queue size, or timeout relationship is a startup error; it does
not fall back to a default. Empty values are rejected where a value is required.

All durations below are milliseconds.

## Kafka

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_KAFKA_BROKERS` | `localhost:9092` | comma-separated `host:port` bootstrap brokers |
| `PULSE_KAFKA_JOBS_TOPIC` | `pulse.scenario.jobs` | source job topic |
| `PULSE_KAFKA_RESULTS_TOPIC` | `pulse.scenario.results` | per-slice result topic |
| `PULSE_KAFKA_SUMMARIES_TOPIC` | `pulse.scenario.summaries` | revisioned aggregate summary topic |
| `PULSE_KAFKA_DLQ_TOPIC` | `pulse.scenario.dlq` | permanent/poison record topic |
| `PULSE_KAFKA_GROUP_ID` | `pulse-workers` | worker consumer group |
| `PULSE_KAFKA_AGGREGATOR_GROUP_ID` | `pulse-aggregators` | result-aggregation consumer group; must differ from the worker group |
| `PULSE_KAFKA_MAX_POLL_INTERVAL_MS` | `300000` | maximum processing/poll interval |
| `PULSE_KAFKA_SESSION_TIMEOUT_MS` | `10000` | consumer session timeout |
| `PULSE_KAFKA_MESSAGE_TIMEOUT_MS` | `10000` | producer message timeout |
| `PULSE_KAFKA_DELIVERY_TIMEOUT_MS` | `10000` | producer delivery timeout; must equal message timeout |
| `PULSE_KAFKA_REQUEST_TIMEOUT_MS` | `5000` | broker request timeout |
| `PULSE_KAFKA_PRODUCER_ACKS` | `all` | only `all`/`-1` is accepted |
| `PULSE_KAFKA_PRODUCER_IDEMPOTENCE` | `true` | librdkafka idempotent producer mode |
| `PULSE_KAFKA_PRODUCER_QUEUE_MESSAGES` | `1024` | producer queue message count |
| `PULSE_KAFKA_PRODUCER_MESSAGE_MAX_BYTES` | `1000000` | maximum serialized producer record bytes |
| `PULSE_KAFKA_CONSUMER_QUEUE_KBYTES` | `4096` | consumer prefetch memory in KiB |
| `PULSE_KAFKA_CONSUMER_PARTITION_FETCH_MAX_BYTES` | `524288` | initial per-partition fetch bytes |
| `PULSE_KAFKA_CONSUMER_FETCH_MAX_BYTES` | `4194304` | total bytes per fetch response |
| `PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES` | `1000000` | hard Pulse limit for one consumed key plus payload before decode/queue ownership |
| `PULSE_KAFKA_TOPIC_MANAGEMENT_ENABLED` | `false` | opt-in topic creation |
| `PULSE_KAFKA_TOPIC_PARTITIONS` | `3` | partitions when management is enabled |
| `PULSE_KAFKA_TOPIC_REPLICATION_FACTOR` | `1` | replication when management is enabled; demo default only |

Jobs/results/summaries/DLQ topics must be distinct. The max poll interval must exceed
the validated worst-case retry deferral, job, scenario, and Kafka-settlement timeout
budget plus a one-second polling safety margin. Both job processing and result
aggregation fail-stop inside that Kafka-safe interval so their source offsets remain
unsettled rather than silently violating `max.poll.interval.ms`.
Production operators should pre-provision topics and leave topic management disabled. In
that mode startup remains unready until read-only metadata probes confirm all four topics
exist and every advertised partition has a leader and no metadata error; Pulse never
auto-creates a missing topic from this check.

The total fetch bound must be at least both the per-partition bound and consumed-record
bound. The consumed-record bound must cover every record Pulse can publish. Oversized
input is classified as deterministic poison before full owned copies are retained.
Poison records retain
deterministic prefixes of at most 256 KiB each for the source key and payload, plus the
original byte counts and explicit truncation flags. Producer message bytes must cover
that fixed base64 evidence envelope; Pulse rejects inconsistent values at startup.
`fetch.message.max.bytes` is Kafka's initial per-partition fetch size, not an absolute
broker-side record limit: Kafka may return an oversized first batch to let a consumer
make progress. Pulse also sets `receive.message.max.bytes` from the total fetch bound
and rejects a record above `PULSE_KAFKA_CONSUMER_RECORD_MAX_BYTES`. Managed Kafka
broker and topic `message.max.bytes`/`max.message.bytes`
limits remain a required hard bound and must align with Pulse's producer and consumer
settings. The defaults keep bounded poison evidence within the demo broker's default
message-size envelope.

## Redis Coordination

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_REDIS_URL` | `redis://127.0.0.1:6379` | Redis URL |
| `PULSE_REDIS_URL_FILE` | unset | file containing the Redis URL; do not set with `PULSE_REDIS_URL` |
| `PULSE_REDIS_LEADER_KEY` | `pulse:{coordination}:leader` | leader lease key; hash tag must match the schedule prefix so multi-key scripts are slot-compatible (native Cluster discovery is not yet supported) |
| `PULSE_REDIS_SCHEDULE_PREFIX` | `pulse:{coordination}:schedule` | dispatch-ledger prefix sharing the leader hash tag |
| `PULSE_REDIS_IDEMPOTENCY_PREFIX` | `pulse:dedupe` | execution lease/terminal prefix |
| `PULSE_REDIS_AGGREGATION_PREFIX` | `pulse:aggregation` | run state, deadline, dedupe, and summary-outbox prefix |
| `PULSE_NODE_ID` | `node-<pid>` | node label; ownership still uses opaque tokens |
| `PULSE_LEADER_LOCK_TTL_MS` | `10000` | leader lease TTL; must cover renewal and Redis response budgets |
| `PULSE_LEADER_RENEW_INTERVAL_MS` | `3000` | leader renewal cadence |
| `PULSE_SCHEDULER_TICK_INTERVAL_MS` | `500` | due-window scan cadence |
| `PULSE_EXECUTION_LEASE_TTL_MS` | `30000` | renewable execution lease TTL; must cover renewal and Redis response budgets |
| `PULSE_EXECUTION_LEASE_RENEW_INTERVAL_MS` | `10000` | execution renewal cadence |
| `PULSE_EXECUTION_TERMINAL_RETENTION_MS` | `86400000` | completed-outcome retention |

Each TTL must cover both three renewal intervals and the Redis operation timeout plus
one renewal interval and a positive margin. Redis operations use the smaller of two
seconds and `PULSE_KAFKA_REQUEST_TIMEOUT_MS`. This prevents a slow successful Redis
response from consuming the lease before Pulse acts on it. Terminal retention must
cover at least the Kafka max poll interval. Schedule, execution, and aggregation
prefixes must be distinct.

Kafka retention is part of the guarantee boundary even though Pulse cannot configure
it when topic management is disabled. Keep the results and DLQ topics at least as long
as `PULSE_EXECUTION_TERMINAL_RETENTION_MS`, keep summaries at least as long as
`PULSE_AGGREGATION_RETENTION_MS`, and size jobs-topic retention for the longest
supported outage/recovery window. Otherwise Redis may still prove that an output was
once acknowledged after Kafka has expired the corresponding evidence.

## Distributed Aggregation

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_AGGREGATION_ENABLED` | `true` | initialize the result consumer, Redis aggregation store, and summary publisher |
| `PULSE_AGGREGATION_PARTIAL_TIMEOUT_MS` | `60000` | grace after the registered load window before an incomplete run becomes `timed_out` (first-result fallback for legacy/unregistered runs) |
| `PULSE_AGGREGATION_RETENTION_MS` | `86400000` | retained aggregate/dedupe/outbox state; must exceed the partial timeout |
| `PULSE_AGGREGATION_SCAN_INTERVAL_MS` | `1000` | deadline and summary-outbox maintenance cadence; must not exceed the partial timeout |
| `PULSE_AGGREGATION_SCAN_BATCH` | `128` | bounded due-run and pending-outbox batch size |
| `PULSE_AGGREGATION_MAX_ACTIVE_RUNS` | `10000` | hard bound on concurrently retained active runs |
| `PULSE_AGGREGATION_MAX_ERROR_KINDS` | `64` | hard bound on distinct error classes retained per run |

The aggregator synchronously commits a result offset only after Redis atomically
accepts or identifies the deterministic slice as a duplicate. Complete and timed-out
summary revisions are stored in a Redis outbox before Kafka publication. A late slice
may advance a timed-out run to a new complete revision; an acknowledgement for an older
revision cannot clear the newer outbox entry. Disabling aggregation also disables the
summary consumer/publisher and should be an explicit operational decision.

## Retry, Startup, and Shutdown

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_WORKER_MAX_RETRIES` | `2` | automatic infrastructure retry ceiling and bounded local settlement retries |
| `PULSE_WORKER_RETRY_BASE_DELAY_MS` | `500` | exponential backoff base |
| `PULSE_WORKER_RETRY_MAX_DELAY_MS` | `30000` | bounded backoff ceiling |
| `PULSE_RETRY_QUEUE_CAPACITY` | `1024` | bounded consumer-to-worker handoff |
| `PULSE_STARTUP_DEADLINE_MS` | `60000` | dependency/plan startup deadline |
| `PULSE_SHUTDOWN_DRAIN_TIMEOUT_MS` | `30000` | bounded in-flight work drain; process join reserves an additional derived coordination/broker cleanup grace |
| `PULSE_ALLOW_PARTIAL_START` | `false` | permit explicitly logged scenario exclusions; zero valid plans still fails |

Classified Pulse infrastructure failures persist attempt N+1 in the normal Kafka jobs
topic with `not_before_unix_ms` before committing attempt N. There is no separate
delayed topic: the bounded, shutdown-aware local not-before wait can head-of-line block
one processor. If retry/DLQ publication or terminal recording exhausts its bounded
local budget, the source remains uncommitted and the worker fail-stops.

## Scenarios and gRPC

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_ENDPOINT` | `http://127.0.0.1:8080` | default target endpoint |
| `PULSE_SCENARIOS_FILE` | `./scenarios.yaml` when present | scenario YAML path |
| `PULSE_GRPC_DESCRIPTOR_SET` | unset | required for dynamic gRPC scenarios |
| `PULSE_GRPC_CONNECT_TIMEOUT_MS` | `5000` | target connection timeout |
| `PULSE_GRPC_REQUEST_TIMEOUT_MS` | `5000` | per-step unary request deadline |
| `PULSE_GRPC_SCENARIO_TIMEOUT_MS` | `30000` | whole scenario execution deadline |
| `PULSE_MAX_DURATION_MS` | `60000` | maximum job load window |
| `PULSE_MAX_SCENARIOS_PER_SEC` | `1000` | finite positive rate ceiling |
| `PULSE_MAX_CONCURRENCY` | `256` | global concurrency ceiling |
| `PULSE_STARTUP_BURST` | `0` | explicit initial token burst |
| `PULSE_DRY_RUN` | `false` | print deterministic slice/load plan and exit without dependencies or traffic |
| `PULSE_TARGET_ALLOWLIST` | `localhost,127.0.0.1,::1` | comma-separated exact target hosts |
| `PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS` | `false` | broad explicit safety override |

The descriptor, service, method, endpoint, and scenario limits are checked before the
runtime becomes ready. Dynamic calls are unary and plaintext HTTP/2 (`http://`) only.
This build has no tonic TLS transport and rejects `https://` endpoints at startup;
custom CA, mTLS, and per-request authentication metadata are not configurable. Use the
target transport only on a trusted network.

## Operations Listener

| Variable | Default | Meaning |
| --- | ---: | --- |
| `PULSE_METRICS_ENABLED` | `true` | enable the combined metrics/health listener |
| `PULSE_METRICS_BIND` | `0.0.0.0:9090` | numeric bind address |

The listener serves `/health/live`, `/health/ready`, and `/metrics` as separate routes.
Disabling it removes all three routes and is unsuitable for the provided Kubernetes
probes.
