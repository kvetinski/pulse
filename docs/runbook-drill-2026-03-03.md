# Runbook Drill - 2026-03-03

## Goal

Validate operational guardrails introduced for overlay-based deployments and capture command/output examples tied to `docs/runbook.md`.

## Scope

- Validate each overlay renders cleanly.
- Validate overlay context/namespace guard behavior in Makefile.
- Validate explicit override path for exceptional cases.

## Environment

- Date: 2026-03-03
- Host: local dev workspace
- Kubernetes context used in checks: `kind-account`
- Overlay namespace defaults:
  - `kind -> pulse-dev`
  - `staging -> pulse-staging`
  - `prod -> pulse-prod`

## Drill Steps and Evidence

## 1) Render all overlays

Command:
```bash
kubectl kustomize k8s >/tmp/root.out && \
kubectl kustomize k8s/overlays/kind >/tmp/kind.out && \
kubectl kustomize k8s/overlays/staging >/tmp/staging.out && \
kubectl kustomize k8s/overlays/prod >/tmp/prod.out
```

Result:
- Success for all four render commands.
- Confirms kustomize graph consistency after base/overlay split.

## 2) Verify guard blocks wrong namespace for overlay

Command:
```bash
make -s k8s-guard-overlay K8S_OVERLAY=prod KUBE_NAMESPACE=pulse-dev KUBE_CONTEXT=kind-account
```

Observed output:
```text
KUBE_NAMESPACE='pulse-dev' does not match overlay default 'pulse-prod' for K8S_OVERLAY='prod'.
Use ALLOW_K8S_ENV_OVERRIDE=true only if this is intentional.
make: *** [Makefile:150: k8s-guard-overlay] Error 1
```

Interpretation:
- Safety check works and prevents accidental cross-environment deploy.

## 3) Verify intentional override path

Command:
```bash
make -s k8s-guard-overlay K8S_OVERLAY=prod KUBE_NAMESPACE=pulse-dev KUBE_CONTEXT=kind-account ALLOW_K8S_ENV_OVERRIDE=true && echo guard_override_ok
```

Observed output:
```text
guard_override_ok
```

Interpretation:
- Operator can bypass guard deliberately for controlled exceptions.

## 4) Verify default mapping per overlay

Commands:
```bash
make -s k8s-guard-overlay K8S_OVERLAY=kind && echo guard_default_kind_ok
make -s k8s-guard-overlay K8S_OVERLAY=staging && echo guard_default_staging_ok
make -s k8s-guard-overlay K8S_OVERLAY=prod && echo guard_default_prod_ok
```

Observed output:
```text
guard_default_kind_ok
guard_default_staging_ok
guard_default_prod_ok
```

Interpretation:
- Defaults in Makefile are internally consistent for all overlays.

## Findings

- Guardrails reduce risk of deploying an overlay into the wrong namespace.
- Overlay manifests render correctly and keep namespace scoping coherent.
- Runbook command style should use `<ns>` placeholders, not hardcoded `pulse`.

## Follow-up Actions

1. Apply per-overlay PrometheusRule where Prometheus Operator is installed.
2. Run a live incident drill in a non-sandbox environment:
   - restart `deployment/kafka` during load
   - verify alert firing
   - verify runbook response timings
