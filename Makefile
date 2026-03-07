SHELL := /bin/bash

CARGO ?= cargo
PROTOC ?= protoc
RUST_LOG ?= info
VERSION ?=
K8S_OVERLAY ?= kind
K8S_OVERLAY_DIR ?= k8s/overlays/$(K8S_OVERLAY)
K8S_KIND_CONTEXT ?= kind-account
K8S_STAGING_CONTEXT ?= kind-account
K8S_PROD_CONTEXT ?= kind-account
K8S_KIND_NAMESPACE ?= pulse-dev
K8S_STAGING_NAMESPACE ?= pulse-staging
K8S_PROD_NAMESPACE ?= pulse-prod
EXPECTED_KUBE_CONTEXT = $(if $(filter kind,$(K8S_OVERLAY)),$(K8S_KIND_CONTEXT),$(if $(filter staging,$(K8S_OVERLAY)),$(K8S_STAGING_CONTEXT),$(if $(filter prod,$(K8S_OVERLAY)),$(K8S_PROD_CONTEXT),invalid)))
EXPECTED_KUBE_NAMESPACE = $(if $(filter kind,$(K8S_OVERLAY)),$(K8S_KIND_NAMESPACE),$(if $(filter staging,$(K8S_OVERLAY)),$(K8S_STAGING_NAMESPACE),$(if $(filter prod,$(K8S_OVERLAY)),$(K8S_PROD_NAMESPACE),invalid)))
KUBE_CONTEXT ?= $(EXPECTED_KUBE_CONTEXT)
KUBE_NAMESPACE ?= $(EXPECTED_KUBE_NAMESPACE)
ALLOW_K8S_ENV_OVERRIDE ?= false
KIND_CLUSTER ?= account
# `kind` CLI expects cluster name (e.g. `account`), not context (`kind-account`).
KIND_CLUSTER_NAME ?= $(if $(filter kind-%,$(KUBE_CONTEXT)),$(patsubst kind-%,%,$(KUBE_CONTEXT)),$(KIND_CLUSTER))
LOCAL_IMAGE ?= pulse:local
KAFKA_IMAGE ?= apache/kafka:3.9.0
REDIS_IMAGE ?= redis:7-alpine
PROMETHEUS_IMAGE ?= prom/prometheus:v2.54.1
GRAFANA_IMAGE ?= grafana/grafana:11.2.0
REGISTRY ?=
IMAGE_REPO ?= pulse
IMAGE_TAG ?= latest
IMAGE ?= $(if $(REGISTRY),$(REGISTRY)/$(IMAGE_REPO):$(IMAGE_TAG),$(IMAGE_REPO):$(IMAGE_TAG))
PROTO_OUT_DIR ?= descriptors
PROTO_DESCRIPTOR ?= $(PROTO_OUT_DIR)/services.pb
PROTO_SRC_DIRS ?= src
PROTO_IMPORT_DIRS ?= /usr/include
PROTO_FILES ?= src/account.proto
TEST_KAFKA_BROKERS ?= 127.0.0.1:19092
TEST_REDIS_URL ?= redis://127.0.0.1:16379
PROMETHEUS_RULE_FILE ?= k8s/examples/alerts/pulse-prometheusrule.$(K8S_OVERLAY).yaml
SECRET_EXAMPLE_FILE ?= k8s/examples/secrets/pulse-secret.$(K8S_OVERLAY).example.yaml
SOAK_DURATION_SEC ?= 1800
SOAK_SAMPLE_INTERVAL_SEC ?= 30
SOAK_CHAOS_PLAN ?= kafka,redis,pulse
SOAK_REPORT_DIR ?= artifacts/reliability
PERF_WINDOW ?= 30m
PERF_THRESHOLD_FILE ?= k8s/overlays/$(K8S_OVERLAY)/performance-thresholds.csv
PERF_REPORT_DIR ?= artifacts/reliability
PERF_PROM_DEPLOYMENT ?= prometheus
PERF_OVERLAY ?= $(K8S_OVERLAY)
PERF_HISTORY_FILE ?= $(PERF_REPORT_DIR)/perf-history.jsonl
PERF_REPORT_MAX_POINTS ?= 40
PERF_GRAFANA_ANNOTATE ?= false
PERF_GRAFANA_URL ?= http://127.0.0.1:3000
PERF_GRAFANA_DASHBOARD_UID ?= pulse-runtime-metrics
PERF_GRAFANA_USER ?= admin
PERF_GRAFANA_PASSWORD ?= admin
PERF_GRAFANA_TOKEN ?=
PERF_GRAFANA_TIMEOUT_SEC ?= 8
PERF_GRAFANA_VERIFY_TLS ?= true

.PHONY: help start start-release check fmt clippy bench ci-check proto-descriptor proto-descriptor-clean release-tag release-tag-push docker-build docker-build-image docker-push docker-rebuild docker-up docker-down docker-logs test-compose-up test-compose-down test-integration-compose kind-build kind-pull-deps kind-load kind-load-deps k8s-guard-overlay k8s-deploy-kind k8s-deploy k8s-deploy-push k8s-delete k8s-stop-pods k8s-start-pods k8s-logs k8s-status k8s-leader-key k8s-kafka-topics k8s-pf-grafana k8s-apply-hpa-example k8s-apply-pdb-example k8s-apply-networkpolicy-example k8s-show-digest-pinning-example k8s-show-secret-example k8s-apply-secret-example k8s-apply-prometheusrule k8s-delete-prometheusrule k8s-chaos-restart-kafka k8s-chaos-restart-redis k8s-chaos-restart-pulse k8s-soak-chaos k8s-check-performance k8s-fix-metrics-server

help: ## Show available targets
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "%-24s %s\n", $$1, $$2}'

start: ## Start pulse service (debug profile)
	RUST_LOG=$(RUST_LOG) $(CARGO) run

start-release: ## Start pulse service (release profile)
	RUST_LOG=$(RUST_LOG) $(CARGO) run --release

check: ## Run cargo check
	$(CARGO) check

fmt: ## Run cargo fmt
	$(CARGO) fmt

clippy: ## Run cargo clippy with warnings as errors
	$(CARGO) clippy --all-targets --all-features -- -D warnings

bench: ## Run benchmark binary (override env: PULSE_BENCH_* iterations/thresholds)
	$(CARGO) run --release --bin pulse_bench

ci-check: ## Full local quality gates used in CI
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	$(CARGO) test --locked --all-targets --all-features
	PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=200 \
	PULSE_BENCH_RUNNER_ITERATIONS=5 \
	PULSE_BENCH_MIN_STARTED_PER_SEC=120 \
	PULSE_BENCH_MAX_AVG_RUN_MS=200 \
	PULSE_BENCH_MAX_DROP_RATIO=0 \
	$(CARGO) run --locked --release --bin pulse_bench
	docker compose config -q
	$(MAKE) proto-descriptor

proto-descriptor: ## Build descriptor set (override PROTO_FILES/PROTO_SRC_DIRS/PROTO_IMPORT_DIRS)
	@mkdir -p $(PROTO_OUT_DIR)
	$(PROTOC) \
		$(foreach d,$(PROTO_SRC_DIRS),-I $(d)) \
		$(foreach d,$(PROTO_IMPORT_DIRS),-I $(d)) \
		--include_imports \
		--include_source_info \
		--descriptor_set_out=$(PROTO_DESCRIPTOR) \
		$(PROTO_FILES)
	@echo "descriptor written to $(PROTO_DESCRIPTOR)"

proto-descriptor-clean: ## Remove generated descriptor set
	rm -f $(PROTO_DESCRIPTOR)

release-tag: ## Create annotated semantic version tag (usage: make release-tag VERSION=0.1.0)
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required (example: make release-tag VERSION=0.1.0)"; exit 1; fi
	@if ! printf '%s\n' "$(VERSION)" | grep -Eq '^[0-9]+\\.[0-9]+\\.[0-9]+$$'; then echo "VERSION must be semantic x.y.z"; exit 1; fi
	@if [ -n "$$(git status --porcelain)" ]; then echo "working tree is not clean; commit or stash changes before tagging"; exit 1; fi
	@if git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null; then echo "tag v$(VERSION) already exists"; exit 1; fi
	git tag -a "v$(VERSION)" -m "Release v$(VERSION)"
	@echo "created tag v$(VERSION)"

release-tag-push: ## Push semantic version tag to origin (usage: make release-tag-push VERSION=0.1.0)
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required (example: make release-tag-push VERSION=0.1.0)"; exit 1; fi
	git push origin "v$(VERSION)"

docker-build: ## Build Docker image via compose
	docker compose build pulse

docker-build-image: ## Build app image tag used for k8s (IMAGE/REGISTRY/IMAGE_REPO/IMAGE_TAG)
	docker build -t $(IMAGE) .

docker-push: ## Push IMAGE to registry (set REGISTRY, e.g. ghcr.io/org)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for docker-push"; exit 1; fi
	docker push $(IMAGE)

docker-rebuild: ## Rebuild Docker image without cache
	docker compose build --no-cache pulse

docker-up: ## Build (if needed) and start full Docker Compose stack (pulse+kafka+redis+prometheus+grafana)
	docker compose up -d --build

docker-down: ## Stop pulse in Docker Compose
	docker compose down

docker-logs: ## Tail logs for pulse, prometheus, and grafana
	docker compose logs -f pulse prometheus grafana

test-compose-up: ## Start Kafka and Redis for docker-backed integration tests
	docker compose up -d --wait kafka redis

test-compose-down: ## Stop test dependencies
	docker compose down --remove-orphans

test-integration-compose: test-compose-up ## Run ignored integration tests against docker compose dependencies
	PULSE_TEST_KAFKA_BROKERS=$(TEST_KAFKA_BROKERS) \
	PULSE_TEST_REDIS_URL=$(TEST_REDIS_URL) \
	$(CARGO) test --locked --test integration_compose -- --ignored --nocapture

kind-build: ## Build local image for kind (LOCAL_IMAGE=pulse:local)
	docker build -t $(LOCAL_IMAGE) .

kind-pull-deps: ## Ensure dependency images exist locally before kind load
	docker image inspect $(KAFKA_IMAGE) >/dev/null 2>&1 || docker pull $(KAFKA_IMAGE)
	docker image inspect $(REDIS_IMAGE) >/dev/null 2>&1 || docker pull $(REDIS_IMAGE)
	docker image inspect $(PROMETHEUS_IMAGE) >/dev/null 2>&1 || docker pull $(PROMETHEUS_IMAGE)
	docker image inspect $(GRAFANA_IMAGE) >/dev/null 2>&1 || docker pull $(GRAFANA_IMAGE)

kind-load: ## Load local image into kind cluster
	kind load docker-image $(LOCAL_IMAGE) --name $(KIND_CLUSTER_NAME)

kind-load-deps: ## Load kafka/redis images into kind cluster
	$(MAKE) kind-pull-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) PROMETHEUS_IMAGE=$(PROMETHEUS_IMAGE) GRAFANA_IMAGE=$(GRAFANA_IMAGE)
	kind load docker-image $(KAFKA_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(REDIS_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(PROMETHEUS_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(GRAFANA_IMAGE) --name $(KIND_CLUSTER_NAME)

k8s-deploy-kind: ## Build + load local kind image, then deploy using kind overlay
	$(MAKE) kind-build LOCAL_IMAGE=$(LOCAL_IMAGE)
	$(MAKE) kind-load-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) PROMETHEUS_IMAGE=$(PROMETHEUS_IMAGE) GRAFANA_IMAGE=$(GRAFANA_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) kind-load LOCAL_IMAGE=$(LOCAL_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) k8s-deploy K8S_OVERLAY=kind

k8s-guard-overlay: ## Validate overlay/context/namespace alignment before kubectl operations
	@if [ "$(EXPECTED_KUBE_CONTEXT)" = "invalid" ] || [ "$(EXPECTED_KUBE_NAMESPACE)" = "invalid" ]; then \
		echo "invalid K8S_OVERLAY='$(K8S_OVERLAY)' (expected: kind|staging|prod)"; \
		exit 1; \
	fi
	@if [ "$(ALLOW_K8S_ENV_OVERRIDE)" != "true" ] && [ "$(KUBE_CONTEXT)" != "$(EXPECTED_KUBE_CONTEXT)" ]; then \
		echo "KUBE_CONTEXT='$(KUBE_CONTEXT)' does not match overlay default '$(EXPECTED_KUBE_CONTEXT)' for K8S_OVERLAY='$(K8S_OVERLAY)'."; \
		echo "Use ALLOW_K8S_ENV_OVERRIDE=true only if this is intentional."; \
		exit 1; \
	fi
	@if [ "$(ALLOW_K8S_ENV_OVERRIDE)" != "true" ] && [ "$(KUBE_NAMESPACE)" != "$(EXPECTED_KUBE_NAMESPACE)" ]; then \
		echo "KUBE_NAMESPACE='$(KUBE_NAMESPACE)' does not match overlay default '$(EXPECTED_KUBE_NAMESPACE)' for K8S_OVERLAY='$(K8S_OVERLAY)'."; \
		echo "Use ALLOW_K8S_ENV_OVERRIDE=true only if this is intentional."; \
		exit 1; \
	fi

k8s-deploy: k8s-guard-overlay ## Deploy pulse to Kubernetes from selected overlay (K8S_OVERLAY=kind|staging|prod)
	kubectl --context $(KUBE_CONTEXT) create namespace $(KUBE_NAMESPACE) --dry-run=client -o yaml | kubectl --context $(KUBE_CONTEXT) apply -f -
	kubectl --context $(KUBE_CONTEXT) apply -k $(K8S_OVERLAY_DIR)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/prometheus
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/grafana
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse

k8s-deploy-push: ## Build + push image, then deploy (requires REGISTRY)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for k8s-deploy-push"; exit 1; fi
	$(MAKE) docker-build-image REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) docker-push REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) k8s-deploy KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE) K8S_OVERLAY=$(K8S_OVERLAY)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/pulse pulse=$(IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse

k8s-delete: k8s-guard-overlay ## Remove pulse resources from Kubernetes
	kubectl --context $(KUBE_CONTEXT) delete -k $(K8S_OVERLAY_DIR) --ignore-not-found=true

k8s-stop-pods: k8s-guard-overlay ## Stop all pods in namespace by scaling deployments/statefulsets to 0 (PVC data is preserved)
	@set -e; \
	echo "stopping workloads in namespace $(KUBE_NAMESPACE) on context $(KUBE_CONTEXT)"; \
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) get deployment -o name | xargs -r -n1 kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) scale --replicas=0; \
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) get statefulset -o name | xargs -r -n1 kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) scale --replicas=0; \
	echo "all scalable workloads are stopped; PVC-backed data is unchanged"

k8s-start-pods: k8s-deploy ## Restore workloads in namespace to overlay-defined replica counts

k8s-logs: k8s-guard-overlay ## Tail pulse pod logs from Kubernetes
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) logs -f deployment/pulse

k8s-status: k8s-guard-overlay ## Show pulse deployment and pod status
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) get deploy,pod -l app=pulse -o wide

k8s-leader-key: k8s-guard-overlay ## Show current redis leader key value (requires redis pod/service name 'redis')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/redis -- redis-cli GET pulse:leader

k8s-kafka-topics: k8s-guard-overlay ## List kafka topics (requires kafka deployment name 'kafka')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list

k8s-pf-grafana: k8s-guard-overlay ## Port-forward Grafana UI to localhost:3001
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) port-forward svc/grafana 3001:3000

k8s-apply-hpa-example: k8s-guard-overlay ## Apply sample HPA (requires metrics-server)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) apply -f k8s/examples/hpa-pulse.yaml

k8s-apply-pdb-example: k8s-guard-overlay ## Apply sample stricter PDB (minAvailable=2)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) apply -f k8s/examples/pdb-pulse.yaml

k8s-apply-networkpolicy-example: k8s-guard-overlay ## Apply sample NetworkPolicy for pulse runtime traffic
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) apply -f k8s/examples/networkpolicy-pulse.yaml

k8s-show-digest-pinning-example: ## Show image digest pinning snippet for kustomization.yaml
	cat k8s/examples/image-digests.example.yaml

k8s-show-secret-example: k8s-guard-overlay ## Show per-overlay pulse secret example manifest
	@if [ ! -f "$(SECRET_EXAMPLE_FILE)" ]; then echo "missing secret example file: $(SECRET_EXAMPLE_FILE)"; exit 1; fi
	cat $(SECRET_EXAMPLE_FILE)

k8s-apply-secret-example: k8s-guard-overlay ## Apply per-overlay pulse secret example manifest
	@if [ ! -f "$(SECRET_EXAMPLE_FILE)" ]; then echo "missing secret example file: $(SECRET_EXAMPLE_FILE)"; exit 1; fi
	kubectl --context $(KUBE_CONTEXT) apply -f $(SECRET_EXAMPLE_FILE)

k8s-apply-prometheusrule: k8s-guard-overlay ## Apply per-overlay PrometheusRule alerts (requires Prometheus Operator CRD)
	@if [ ! -f "$(PROMETHEUS_RULE_FILE)" ]; then echo "missing PrometheusRule file: $(PROMETHEUS_RULE_FILE)"; exit 1; fi
	@if ! kubectl --context $(KUBE_CONTEXT) get crd prometheusrules.monitoring.coreos.com >/dev/null 2>&1; then \
		echo "CRD prometheusrules.monitoring.coreos.com is not installed in cluster $(KUBE_CONTEXT)."; \
		echo "Install Prometheus Operator/kube-prometheus-stack first."; \
		exit 1; \
	fi
	kubectl --context $(KUBE_CONTEXT) apply -f $(PROMETHEUS_RULE_FILE)

k8s-delete-prometheusrule: k8s-guard-overlay ## Delete per-overlay PrometheusRule alerts
	@if [ ! -f "$(PROMETHEUS_RULE_FILE)" ]; then echo "missing PrometheusRule file: $(PROMETHEUS_RULE_FILE)"; exit 1; fi
	@if ! kubectl --context $(KUBE_CONTEXT) get crd prometheusrules.monitoring.coreos.com >/dev/null 2>&1; then \
		echo "CRD prometheusrules.monitoring.coreos.com is not installed in cluster $(KUBE_CONTEXT)."; \
		exit 1; \
	fi
	kubectl --context $(KUBE_CONTEXT) delete -f $(PROMETHEUS_RULE_FILE) --ignore-not-found=true

k8s-chaos-restart-kafka: k8s-guard-overlay ## Restart Kafka deployment and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/kafka --timeout=300s

k8s-chaos-restart-redis: k8s-guard-overlay ## Restart Redis deployment and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/redis --timeout=300s

k8s-chaos-restart-pulse: k8s-guard-overlay ## Restart Pulse deployment and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/pulse
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse --timeout=300s

k8s-soak-chaos: k8s-guard-overlay ## Run soak test with planned chaos restarts (SOAK_DURATION_SEC/SOAK_SAMPLE_INTERVAL_SEC/SOAK_CHAOS_PLAN)
	KUBE_CONTEXT=$(KUBE_CONTEXT) \
	KUBE_NAMESPACE=$(KUBE_NAMESPACE) \
	SOAK_DURATION_SEC=$(SOAK_DURATION_SEC) \
	SOAK_SAMPLE_INTERVAL_SEC=$(SOAK_SAMPLE_INTERVAL_SEC) \
	SOAK_CHAOS_PLAN=$(SOAK_CHAOS_PLAN) \
	SOAK_REPORT_DIR=$(SOAK_REPORT_DIR) \
	bash scripts/reliability/soak_chaos.sh

k8s-check-performance: k8s-guard-overlay ## Enforce runtime performance thresholds from overlay CSV via Prometheus
	KUBE_CONTEXT=$(KUBE_CONTEXT) \
	KUBE_NAMESPACE=$(KUBE_NAMESPACE) \
	PERF_PROM_DEPLOYMENT=$(PERF_PROM_DEPLOYMENT) \
	PERF_OVERLAY=$(PERF_OVERLAY) \
	PERF_WINDOW=$(PERF_WINDOW) \
	PERF_THRESHOLD_FILE=$(PERF_THRESHOLD_FILE) \
	PERF_REPORT_DIR=$(PERF_REPORT_DIR) \
	PERF_HISTORY_FILE=$(PERF_HISTORY_FILE) \
	PERF_REPORT_MAX_POINTS=$(PERF_REPORT_MAX_POINTS) \
	PERF_GRAFANA_ANNOTATE=$(PERF_GRAFANA_ANNOTATE) \
	PERF_GRAFANA_URL=$(PERF_GRAFANA_URL) \
	PERF_GRAFANA_DASHBOARD_UID=$(PERF_GRAFANA_DASHBOARD_UID) \
	PERF_GRAFANA_USER=$(PERF_GRAFANA_USER) \
	PERF_GRAFANA_PASSWORD=$(PERF_GRAFANA_PASSWORD) \
	PERF_GRAFANA_TOKEN=$(PERF_GRAFANA_TOKEN) \
	PERF_GRAFANA_TIMEOUT_SEC=$(PERF_GRAFANA_TIMEOUT_SEC) \
	PERF_GRAFANA_VERIFY_TLS=$(PERF_GRAFANA_VERIFY_TLS) \
	bash scripts/reliability/check_performance_thresholds.sh

k8s-fix-metrics-server: ## Patch kube-system metrics-server for kind kubelet TLS and verify metrics API
	kubectl --context $(KUBE_CONTEXT) -n kube-system patch deployment metrics-server --type='strategic' -p '{"spec":{"template":{"spec":{"containers":[{"name":"metrics-server","args":["--cert-dir=/tmp","--secure-port=10250","--kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP","--kubelet-use-node-status-port","--metric-resolution=15s","--kubelet-insecure-tls"]}]}}}}'
	kubectl --context $(KUBE_CONTEXT) -n kube-system rollout status deployment/metrics-server
	kubectl --context $(KUBE_CONTEXT) top nodes
