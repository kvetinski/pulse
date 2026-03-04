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
- Pulse Kafka topics (`jobs`, `results`, `dlq`)
- Redis leader/idempotency storage
- Runtime metrics exposed at `/metrics`

## First Response Checklist

1. Confirm deployment health:
   - `kubectl --context <ctx> -n <ns> get deploy,pod -o wide`
2. Check Pulse logs for current failure mode:
   - `kubectl --context <ctx> -n <ns> logs deploy/pulse --tail=200`
3. Verify scheduler leadership metric:
   - `pulse_scheduler_is_leader`
4. Verify Kafka and Redis pods are healthy:
   - `kubectl --context <ctx> -n <ns> get pods -l app=kafka`
   - `kubectl --context <ctx> -n <ns> get pods -l app=redis`
5. Check DLQ growth:
   - `pulse_worker_dlq_published_total`

## Incident Playbooks

## 1) No Active Leader

Symptoms:
- `pulse_scheduler_is_leader == 0` on all pods.
- No new jobs are published (`pulse_scheduler_jobs_published_total` flat).

Actions:
1. Validate Redis reachability from Pulse pods.
2. Check Redis key owner:
   - `kubectl --context <ctx> -n <ns> exec deploy/redis -- redis-cli GET pulse:leader`
3. Restart `pulse` deployment if lock appears stale:
   - `kubectl --context <ctx> -n <ns> rollout restart deployment/pulse`
4. If repeated leader flapping, increase lock TTL and renew interval conservatively.

## 2) Kafka Publish/Consume Failures

Symptoms:
- `pulse_scheduler_job_publish_failures_total` increasing.
- `pulse_worker_job_consume_errors_total` increasing.
- Results throughput drops.

Actions:
1. Check Kafka pod status and logs:
   - `kubectl --context <ctx> -n <ns> logs deploy/kafka --tail=200`
2. Check topic availability:
   - `kubectl --context <ctx> -n <ns> exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list`
3. If broker restarted, watch consumer recovery before manual restarts.
4. If still failing, restart pulse workers:
   - `kubectl --context <ctx> -n <ns> rollout restart deployment/pulse`

## 3) Redis Unavailable / Idempotency Errors

Symptoms:
- Duplicate suppression degrades.
- Leader election unstable.
- Worker processing errors after consume.

Actions:
1. Check Redis pod health and restarts.
2. Confirm `PULSE_REDIS_URL` or `PULSE_REDIS_URL_FILE` is correct.
3. Restore Redis first, then restart pulse to re-establish stable leader/worker behavior.

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
   - unknown scenario
   - retry exhaustion
   - retry publish failure
3. Fix root cause first (schema, endpoint, dependency outage).
4. Replay only idempotent-safe jobs with controlled rate.

## Rollback Procedure

1. Roll back to previous image:
   - `kubectl --context <ctx> -n <ns> rollout undo deployment/pulse`
2. Re-verify:
   - pod readiness
   - scheduler leadership
   - consume/publish error rates
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
