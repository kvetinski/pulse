# Pulse Runbook

This runbook covers first response for Pulse runtime incidents in Kubernetes.

Use the active deployment namespace for your overlay:
- `kind`: `pulse-dev`
- `staging`: `pulse-staging`
- `prod`: `pulse-prod`

Reference drill record:
- `docs/runbook-drill-2026-03-03.md`

## Scope

Applies to:
- Scheduler/worker runtime in `Deployment/pulse`
- Pulse Kafka topics (`jobs`, `results`, `summaries`, `dlq`)
- Redis leader, dispatch, execution, and aggregation/outbox storage
- Runtime health at `/health/live` and `/health/ready`
- Runtime metrics exposed at `/metrics`

The kind overlay owns the deterministic `grpc-target` plus demo Kafka, Redis,
Prometheus, and Grafana deployments. Staging and production-oriented overlays are
app-only: use the managed service's tooling and runbooks instead of commands that
assume these demo deployments.

## First Response Checklist

1. Confirm deployment health:
   - `kubectl --context <ctx> -n <ns> get deploy,pod -o wide`
   - `kubectl --context <ctx> -n <ns> port-forward svc/pulse 9090:9090`
   - `curl -fsS http://127.0.0.1:9090/health/live`
   - `curl -fsS http://127.0.0.1:9090/health/ready`
2. Check Pulse logs for current failure mode:
   - `kubectl --context <ctx> -n <ns> logs deploy/pulse --tail=200`
3. Verify scheduler leadership metric:
   - `pulse_scheduler_is_leader`
4. Verify Kafka and Redis through managed-service health/lag dashboards. For kind only:
   - `kubectl --context kind-account -n pulse-dev get pods -l app=kafka`
   - `kubectl --context kind-account -n pulse-dev get pods -l app=redis`
5. Check DLQ growth:
   - `pulse_worker_dlq_published_total`
6. Check settlement health:
   - `pulse_worker_result_publish_failures_total`
   - `pulse_worker_dlq_publish_failures_total`
   - `pulse_worker_job_commit_failures_total`
   - consumer lag and age in the Kafka platform dashboard
7. Check aggregation health:
   - `pulse_aggregate_results_total{outcome=~"complete|timed_out|late_complete|duplicate"}`
   - result-consumer lag for `PULSE_KAFKA_AGGREGATOR_GROUP_ID`
   - pending summary state under `PULSE_REDIS_AGGREGATION_PREFIX`

## Incident Playbooks

## 1) No Active Leader

Symptoms:
- `pulse_scheduler_is_leader == 0` on all pods.
- No new jobs are published (`pulse_scheduler_jobs_published_total` flat).

Actions:
1. Validate Redis reachability from Pulse pods.
2. Check Redis leader/dispatch keys with the managed Redis tooling. For kind only:
   - `kubectl --context <ctx> -n <ns> exec deploy/redis -- redis-cli HGETALL 'pulse:{coordination}:leader'`
3. Inspect leader renewal failures and verify the configured TTL covers at least three
   renewal intervals. Do not delete a lock merely because a pod log looks stale; owner
   tokens and TTL expiry fence recovery.
4. Restore Redis connectivity. Pulse must remain unready/backpressured during the
   outage; Redis errors are never accepted as `not due` or `duplicate`.
5. Restart Pulse only after capturing logs if readiness does not recover.

## 2) Kafka Publish/Consume Failures

Symptoms:
- `pulse_scheduler_job_publish_failures_total` increasing.
- `pulse_worker_job_consume_errors_total` increasing.
- Results throughput drops.

Actions:
1. Check the managed Kafka service, broker acknowledgements, partition leadership, and
   consumer lag. For kind only, inspect the demo broker:
   - `kubectl --context <ctx> -n <ns> logs deploy/kafka --tail=200`
2. Check topic availability:
   - `kubectl --context <ctx> -n <ns> exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list`
3. If a broker restarted, watch consumer recovery and synchronous commit failures
   before manual restarts. A publication failure must leave the source unsettled.
4. If Pulse readiness does not recover after Kafka, capture logs and restart workers:
   - `kubectl --context <ctx> -n <ns> rollout restart deployment/pulse`

## Active Dispatch Window Rejects a Changed Plan

Symptoms:

- logs repeatedly report `dispatch coordination failed` with an invalid-state or plan
  fingerprint mismatch for one scenario;
- that scenario's incomplete-slice metric remains non-zero even though process
  readiness is green; and
- a scenario, startup-burst, partition strategy, or worker retry-ceiling change was
  rolled out while a window was incomplete.

Actions:

1. Stop further configuration rollout and retain the Redis ledger plus Kafka job
   records as incident evidence. Do not delete the active ledger.
2. Restore the exact prior scenario and job-payload settings, including
   `PULSE_STARTUP_BURST` and `PULSE_WORKER_MAX_RETRIES`.
3. Allow the prior window's deterministic missing slices to publish and confirm the
   incomplete-slice metric reaches zero.
4. Roll out the new plan only after that window completes.
5. If the prior plan cannot be restored, stop all Pulse replicas and make an explicit
   duplicate-traffic decision. Starting with a new versioned
   `PULSE_REDIS_SCHEDULE_PREFIX` abandons the old namespace and may schedule a new
   once-only run; archive the old Redis/Kafka evidence and record that acceptance.

Current readiness represents dependency and runtime-loop health, not compatibility of
every retained per-scenario dispatch ledger. Alert on incomplete dispatch age and the
invalid-state log until a dedicated blocked-scenario readiness state is implemented.

## 3) Redis Unavailable / Idempotency Errors

Symptoms:
- `/health/ready` is non-success.
- Leader election unstable.
- Execution claims/renewals fail and consumed work remains uncommitted.

Actions:
1. Check the managed Redis service; for kind, check the demo pod and restarts.
2. Confirm `PULSE_REDIS_URL` or `PULSE_REDIS_URL_FILE` is correct.
3. Confirm no offset commits were inferred from the coordination errors.
4. Restore Redis. Expired execution leases allow redelivery recovery; duplicate target
   execution remains possible at the external-side-effect crash boundary.

## 4) Target Service Saturation (Account)

Symptoms:
- Scenario failure ratio spikes.
- Scenario duration p95/p99 increases significantly.

Actions:
1. Reduce load quickly:
   - Lower `scenarios_per_sec` in scenario YAML and redeploy config.
   - Or reduce pulse replicas temporarily.
2. Validate account service health independently.
3. Resume load gradually and watch p95 latency + error ratio.

## 5) DLQ Growth

Symptoms:
- `pulse_worker_dlq_published_total` rate is non-zero for sustained period.

Actions:
1. Inspect failure reason from DLQ payloads.
2. Identify class:
   - malformed or unsupported Kafka contract
   - unknown scenario/plan on the worker
   - permanent scenario metadata violation
3. Fix root cause first (schema, endpoint, dependency outage).
4. Review DLQ policy and replay checklist in `docs/dlq-operations.md`.
5. Dry-run replay with filters:
   - `PULSE_DLQ_REPLAY_DRY_RUN=true PULSE_DLQ_REPLAY_SCENARIO_IDS=<id> cargo run --bin pulse_dlq`
6. Execute replay (idempotent-only):
   - `PULSE_DLQ_REPLAY_DRY_RUN=false PULSE_DLQ_REPLAY_CONFIRM_IDEMPOTENT=true PULSE_DLQ_REPLAY_SCALE=1.0 cargo run --bin pulse_dlq`

Do not replay when:
- Root cause is still active (dependency outage, bad schema, bad endpoint).
- Scenario is not idempotent or idempotency is not confirmed.
- You cannot control replay record rate with `PULSE_DLQ_REPLAY_RATE_PER_SEC`.
- You require arbitrary per-job load scaling: v2 rejects it because it would violate
  the deterministic local plan contract.

Monitoring checklist during replay:
- `pulse_worker_dlq_published_total` rate
- Scenario failure rate
- Scenario latency p95
- `pulse_worker_result_publish_failures_total`
- `pulse_worker_retry_job_age_seconds`

## 6) Unsettled Jobs / Commit Failures

Symptoms:

- consumer lag or oldest-job age grows;
- `pulse_worker_result_publish_failures_total`,
  `pulse_worker_dlq_publish_failures_total`, or
  `pulse_worker_job_commit_failures_total` increases;
- target traffic may have occurred without a matching source commit.

Actions:

1. Preserve the job `topic/partition/offset`, `execution_key`, `attempt`, and result
   event ID from structured logs.
2. Determine whether Kafka acknowledged an output record. Do not manually advance the
   consumer-group offset to clear lag.
3. Restore Kafka/Redis and allow redelivery. Deterministic keys and aggregation make
   duplicate terminal records safe; external target side effects still require
   target-side idempotency.
4. If a partition remains blocked, reduce incoming scheduling and investigate the
   earliest unsettled record before considering any operator settlement.

## 7) Shutdown Drain Exceeded

Symptoms:

- pod termination reaches the configured drain deadline;
- logs report incomplete work;
- readiness has already failed while liveness still succeeds.

Actions:

1. Avoid shortening Kubernetes termination grace below the Pulse drain timeout.
2. Restore dependency connectivity so terminal publications can finish.
3. After restart, verify redelivery reaches a durable result/DLQ disposition and that
   the aggregate ignores duplicate results.

## 8) Incomplete Runs / Summary Outbox Backlog

Symptoms:

- `pulse_aggregate_results_total{outcome="timed_out"}` increases;
- the result-aggregation consumer group lags;
- per-slice results exist but no corresponding summary revision is visible; or
- logs report that the Redis summary outbox remains pending.

Actions:

1. Compare the summary's `received_slices`, `expected_slices`, and missing indexes with
   deterministic scheduler slice identities. Do not average per-slice quantiles.
2. Check result-topic lag for `PULSE_KAFKA_AGGREGATOR_GROUP_ID` and Redis availability.
   A dependency error must leave the result offset uncommitted.
3. Check Kafka acknowledgement errors on `PULSE_KAFKA_SUMMARIES_TOPIC`. Restore Kafka
   and let the durable Redis outbox republish; do not delete outbox keys to hide lag.
4. Treat repeated `event_id`/revision pairs as duplicates. A later complete revision
   supersedes an earlier timed-out or partial summary without erasing its audit trail.
5. If retained state has expired, record the run as irrecoverably incomplete; do not
   synthesize a complete aggregate from per-slice p95/p99 values.

## Rollback Procedure

1. Roll back to previous image:
   - `kubectl --context <ctx> -n <ns> rollout undo deployment/pulse`
2. Re-verify:
   - pod readiness
   - scheduler leadership
   - consume/publish error rates
   - consumer lag is converging and result counts are complete
3. Keep load reduced until metrics stabilize.

## Escalation Policy

Escalate immediately when any condition holds:
- No scheduler leader for > 5 minutes.
- DLQ publish failures are non-zero for > 5 minutes.
- Scenario failure rate > 10% for > 10 minutes.
- Kafka/Redis outage cannot be restored within 15 minutes.

Escalate to:
1. Service owner on-call (Pulse)
2. Platform/SRE on-call (cluster or network issues)
3. Account service owner (target dependency issues)

## On-Call Checklist

Before handoff/end of incident:
- Record timeline, impacted scenarios, and user-visible impact.
- Record top metric deltas (error rate, p95 latency, DLQ rate).
- Record root cause and mitigation.
- Create follow-up issue for prevention (config/code/monitoring).
