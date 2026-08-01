# Performance and Reliability Evidence Policy

Pulse distinguishes repeatable evidence from smoke checks and historical observations.
No result is authoritative unless its environment and completeness can be audited.

## Required Bundle

Store one immutable directory per run containing:

- Git commit, branch/tag, and dirty status;
- `rustc`/`cargo` versions and build profile;
- host/cluster OS, CPU, memory, Kubernetes and Docker versions;
- Pulse image reference and digest;
- scenario YAML and descriptor-set files plus SHA-256 hashes;
- Kafka topic/broker settings and Redis persistence/eviction settings;
- complete Pulse environment with secrets redacted;
- target-service version, replica/resources, configuration, and fixture seed;
- exact UTC start/end timestamps and failure-injection timeline;
- raw Prometheus HTTP responses for every derived number;
- source/result/DLQ counts, unique deterministic identities, aggregate completeness,
  duplicate count, and missing slices;
- pod/process resource snapshots;
- relevant structured logs and exact command lines; and
- explicit limitations, anomalies, excluded intervals, and unavailable observations.

Derived tables/charts belong beside, not instead of, the raw inputs. Hash the final
manifest and do not modify a published bundle in place.

## Capture Workflow

Kind smoke and chaos commands capture evidence by default:

```bash
make k8s-check-performance K8S_OVERLAY=kind PERF_WINDOW=30m
make k8s-soak-chaos K8S_OVERLAY=kind SOAK_DURATION_SEC=1800
```

The performance command writes `artifacts/reliability/evidence-<UTC>/`; the soak
command writes `artifacts/reliability/evidence-soak-<UTC>/`. Each finalized directory
contains a file-level hash list, a JSON manifest, and a hash of that manifest. Missing
observations are recorded in `limitations.txt` instead of being silently omitted.

For another controlled window, use the generic two-phase collector around the exact
commands under test:

```bash
EVIDENCE_CLASS=local_observation \
EVIDENCE_BUILD_PROFILE=release \
EVIDENCE_SCENARIO_FILES=scenarios.yaml \
EVIDENCE_DESCRIPTOR_FILES=descriptors/services.pb \
scripts/reliability/capture_evidence_bundle.sh start artifacts/reliability/evidence-manual

# Run the workload and append fault events to failure-timeline.jsonl.

EVIDENCE_CLASS=local_observation \
EVIDENCE_BUILD_PROFILE=release \
EVIDENCE_SCENARIO_FILES=scenarios.yaml \
EVIDENCE_DESCRIPTOR_FILES=descriptors/services.pb \
scripts/reliability/capture_evidence_bundle.sh finish artifacts/reliability/evidence-manual
```

Use colon-separated scenario or descriptor paths when a run has multiple inputs. Set
`EVIDENCE_TARGET_DEPLOYMENT` to capture the target manifest and logs, and select
redacted configuration snapshots with `EVIDENCE_PULSE_CONFIGMAP` (default
`pulse-config`) and `EVIDENCE_TARGET_CONFIGMAP`. The collector redacts credential-like
deployment/ConfigMap fields and URI user information, but operators must still inspect
a bundle before sharing it.

The built-in metric summary explicitly does not prove unique source/result/DLQ record
identities. Add a redacted Kafka topic export when an identity-level delivery claim
depends on those records.

## Claim Levels

- **CI smoke check:** catches catastrophic regressions on a noisy shared runner. It is
  not a capacity or throughput claim.
- **Local observation:** useful for development, tied only to the recorded machine.
- **Controlled benchmark:** repeatable dedicated environment, multiple runs, raw data,
  warmup definition, confidence/variance, and complete result accounting.
- **Failure evidence:** controlled fault boundary plus assertions about source offsets,
  terminal outputs, duplicate handling, missing slices, and post-fault recovery.

Absolute throughput thresholds from shared CI must always be labelled smoke checks.
Do not compare historical results after scenario, descriptor, target, dependency,
hardware, or Pulse configuration changes without calling out the mismatch.

## Redaction

Never store passwords, tokens, SASL credentials, private keys, or full sensitive target
payloads. Record the authentication mode, CA/certificate fingerprints where safe, and
the names of redacted variables so the topology remains understandable.

## Historical Data

Existing results in `docs/benchmarks.md` predate the complete bundle format. They are
retained as historical observations, not rewritten or promoted. A new controlled run
must produce a fresh bundle; improved numbers must never be inferred from code changes.
