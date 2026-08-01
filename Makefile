SHELL := /bin/bash

CARGO ?= cargo
COMPOSE_PROJECT_NAME ?= pulse
COMPOSE_NETWORK_NAME ?= $(COMPOSE_PROJECT_NAME)_default
DOCKER_SUBNET_SELECTOR ?= scripts/docker/select_subnet.py
PROTOC ?= protoc
RUST_LOG ?= info
VERSION ?=
K8S_OVERLAY ?= kind
K8S_OVERLAY_DIR ?= k8s/overlays/$(K8S_OVERLAY)
K8S_KIND_CONTEXT ?= kind-account
K8S_STAGING_CONTEXT ?= replace-with-staging-context
K8S_PROD_CONTEXT ?= replace-with-production-context
K8S_KIND_NAMESPACE ?= pulse-dev
K8S_STAGING_NAMESPACE ?= pulse-staging
K8S_PROD_NAMESPACE ?= pulse-prod
K8S_ROLLOUT_TIMEOUT ?= 300s
EXPECTED_KUBE_CONTEXT = $(if $(filter kind,$(K8S_OVERLAY)),$(K8S_KIND_CONTEXT),$(if $(filter staging,$(K8S_OVERLAY)),$(K8S_STAGING_CONTEXT),$(if $(filter prod,$(K8S_OVERLAY)),$(K8S_PROD_CONTEXT),invalid)))
EXPECTED_KUBE_NAMESPACE = $(if $(filter kind,$(K8S_OVERLAY)),$(K8S_KIND_NAMESPACE),$(if $(filter staging,$(K8S_OVERLAY)),$(K8S_STAGING_NAMESPACE),$(if $(filter prod,$(K8S_OVERLAY)),$(K8S_PROD_NAMESPACE),invalid)))
KUBE_CONTEXT ?= $(EXPECTED_KUBE_CONTEXT)
KUBE_NAMESPACE ?= $(EXPECTED_KUBE_NAMESPACE)
ALLOW_K8S_ENV_OVERRIDE ?= false
KIND_CLUSTER ?= account
# `kind` CLI expects cluster name (e.g. `account`), not context (`kind-account`).
KIND_CLUSTER_NAME ?= $(if $(filter kind-%,$(KUBE_CONTEXT)),$(patsubst kind-%,%,$(KUBE_CONTEXT)),$(KIND_CLUSTER))
LOCAL_IMAGE ?= pulse:local
DEMO_TARGET_IMAGE ?= pulse-demo-target:local
KIND_INPUT_DIR ?= artifacts/kind-inputs
KAFKA_IMAGE ?= apache/kafka:3.9.0
REDIS_IMAGE ?= redis:7-alpine
PROMETHEUS_IMAGE ?= prom/prometheus:v2.54.1
GRAFANA_IMAGE ?= grafana/grafana:11.2.2
TRIVY_IMAGE ?= aquasec/trivy:0.72.0
TRIVY_DB_REPOSITORY ?= ghcr.io/aquasecurity/trivy-db:2
SYFT_IMAGE ?= anchore/syft:v1.50.0
CARGO_AUDIT_VERSION ?= 0.22.2
REGISTRY ?=
IMAGE_REPO ?= pulse
IMAGE_TAG ?= 0.2.0
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
SOAK_MIN_JOBS_RECEIVED ?= 1
SOAK_MIN_RESULTS_PUBLISHED ?= 1
SOAK_MIN_SOURCE_COMMITS ?= 1
SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC ?= 120
SOAK_POST_FAULT_POLL_INTERVAL_SEC ?= 5
SOAK_MIN_POST_FAULT_PROGRESS ?= 1
SOAK_EVIDENCE_ENABLED ?= true
SOAK_EVIDENCE_CLASS ?= failure_evidence
SOAK_EVIDENCE_DIR ?=
SOAK_BUILD_PROFILE ?= unknown
SOAK_SCENARIO_FILES ?= k8s/overlays/kind/scenarios.kind.yaml
SOAK_DESCRIPTOR_FILES ?= $(KIND_INPUT_DIR)/demo.pb
SOAK_TARGET_DEPLOYMENT ?= grpc-target
SOAK_PULSE_CONFIGMAP ?= pulse-config
SOAK_TARGET_CONFIGMAP ?=
PERF_WINDOW ?= 30m
PERF_THRESHOLD_FILE ?= k8s/overlays/$(K8S_OVERLAY)/performance-thresholds.csv
PERF_REPORT_DIR ?= artifacts/reliability
SECURITY_REPORT_DIR ?= artifacts/security
TRIVY_CACHE_DIR ?= $(SECURITY_REPORT_DIR)/trivy-cache
TRIVY_REPORT_FILE ?= $(SECURITY_REPORT_DIR)/trivy-image-report.json
SYFT_SBOM_FILE ?= $(SECURITY_REPORT_DIR)/pulse-image-sbom.spdx.json
DEMO_TARGET_TRIVY_REPORT_FILE ?= $(SECURITY_REPORT_DIR)/trivy-demo-target-report.json
DEMO_TARGET_SYFT_SBOM_FILE ?= $(SECURITY_REPORT_DIR)/pulse-demo-target-sbom.spdx.json
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
PERF_EVIDENCE_ENABLED ?= true
PERF_EVIDENCE_CLASS ?= environment_smoke_check
PERF_EVIDENCE_DIR ?=
PERF_BUILD_PROFILE ?= unknown
PERF_SCENARIO_FILES ?= k8s/overlays/kind/scenarios.kind.yaml
PERF_DESCRIPTOR_FILES ?= $(KIND_INPUT_DIR)/demo.pb
PERF_TARGET_DEPLOYMENT ?= grpc-target
PERF_PULSE_CONFIGMAP ?= pulse-config
PERF_TARGET_CONFIGMAP ?=
LAPTOP_CARGO_JOBS ?= 2

.PHONY: help doctor demo-doctor validate-config demo demo-down docs-validate ci start start-release check fmt clippy bench
.PHONY: ci-check ci-check-laptop ci-check-full supply-chain-check supply-chain-check-laptop
.PHONY: proto-descriptor proto-descriptor-clean release-validate release-tag release-tag-push
.PHONY: docker-build docker-build-image docker-push docker-rebuild docker-up docker-down docker-logs
.PHONY: test-compose-up test-compose-down test-integration-compose kind-build kind-pull-deps kind-load kind-load-deps
.PHONY: k8s-validate k8s-guard-overlay k8s-guard-demo-stack k8s-deploy-kind k8s-deploy k8s-deploy-push
.PHONY: k8s-delete k8s-stop-pods k8s-start-pods k8s-logs k8s-status k8s-leader-key k8s-kafka-topics
.PHONY: k8s-pf-grafana k8s-apply-hpa-example k8s-apply-pdb-example k8s-apply-networkpolicy-example
.PHONY: k8s-show-digest-pinning-example k8s-show-secret-example k8s-apply-secret-example
.PHONY: k8s-apply-prometheusrule k8s-delete-prometheusrule k8s-chaos-restart-kafka
.PHONY: k8s-chaos-restart-redis k8s-chaos-restart-pulse k8s-soak-chaos k8s-check-performance k8s-fix-metrics-server

help: ## Show available targets
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "%-24s %s\n", $$1, $$2}'

demo-doctor:
	@set -e; for tool in docker python3 curl; do \
		command -v $$tool >/dev/null 2>&1 || { echo "missing required tool: $$tool"; exit 1; }; \
	done
	@docker compose version >/dev/null
	@docker info >/dev/null 2>&1 || { echo "Docker daemon is not reachable"; exit 1; }
	@echo "Pulse demo prerequisites are available"

doctor: demo-doctor ## Verify the complete local development toolchain
	@set -e; for tool in cargo rustc rust-analyzer protoc; do \
		command -v $$tool >/dev/null 2>&1 || { echo "missing required tool: $$tool"; exit 1; }; \
	done
	@expected="$$(awk -F '"' '/^channel = / { print $$2; exit }' rust-toolchain.toml)"; \
	actual="$$(rustc --version | awk '{ print $$2 }')"; \
	[ "$$actual" = "$$expected" ] || { echo "Rust $$actual is active; expected $$expected"; exit 1; }
	@echo "Pulse development prerequisites are available"

validate-config: proto-descriptor ## Parse, validate, and print the local execution plan without traffic
	PULSE_DRY_RUN=true \
	PULSE_ACKNOWLEDGE_NON_LOCAL_TARGETS=true \
	PULSE_SCENARIOS_FILE=./scenarios.yaml \
	PULSE_GRPC_DESCRIPTOR_SET=./descriptors/services.pb \
	$(CARGO) run --locked --bin pulse

demo: demo-doctor ## Run the isolated local target, dependencies, Pulse, and result verification
	./demo/run.sh

demo-down: ## Stop and remove the isolated local demo stack
	./demo/down.sh

docs-validate: ## Validate documented commands, paths, probes, scripts, and demo Compose
	scripts/docs/validate.sh

ci: ci-check k8s-validate docs-validate ## Run Rust, contract, docs, release, and Kubernetes gates

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
	$(MAKE) release-validate
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	$(CARGO) test --locked --all-targets --all-features
	# Coarse smoke floor only: the fixture's sequential 20 ms runs cap near 50/s.
	PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=200 \
	PULSE_BENCH_RUNNER_ITERATIONS=5 \
	PULSE_BENCH_MIN_STARTED_PER_SEC=20 \
	PULSE_BENCH_MAX_AVG_RUN_MS=200 \
	PULSE_BENCH_MAX_DROP_RATIO=0 \
	$(CARGO) run --locked --release --bin pulse_bench
	docker compose config -q
	$(MAKE) proto-descriptor

ci-check-laptop: ## Lower-CPU local quality checks (no release bench / image build)
	$(CARGO) fmt --all -- --check
	CARGO_BUILD_JOBS=$(LAPTOP_CARGO_JOBS) $(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	CARGO_BUILD_JOBS=$(LAPTOP_CARGO_JOBS) $(CARGO) test --locked --all-targets --all-features
	docker compose config -q
	$(MAKE) proto-descriptor

supply-chain-check: ## Run local supply-chain checks (cargo-audit + trivy scan + SBOM)
	@cargo audit --version 2>/dev/null | grep -Fq "$(CARGO_AUDIT_VERSION)" || cargo install cargo-audit --locked --version $(CARGO_AUDIT_VERSION) --force
	@mkdir -p $(SECURITY_REPORT_DIR) $(TRIVY_CACHE_DIR)
	cargo audit
	cargo audit --file demo/grpc-target/Cargo.lock
	docker build -t pulse:ci .
	docker build -f demo/grpc-target/Dockerfile -t pulse-demo-target:ci .
	docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$$(pwd)":/work -v "$$(pwd)/$(TRIVY_CACHE_DIR)":/root/.cache/trivy $(TRIVY_IMAGE) image --db-repository $(TRIVY_DB_REPOSITORY) --no-progress --scanners vuln --severity HIGH,CRITICAL --exit-code 1 --ignore-unfixed --format json --output /work/$(TRIVY_REPORT_FILE) pulse:ci
	docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$$(pwd)":/work -v "$$(pwd)/$(TRIVY_CACHE_DIR)":/root/.cache/trivy $(TRIVY_IMAGE) image --db-repository $(TRIVY_DB_REPOSITORY) --no-progress --scanners vuln --severity HIGH,CRITICAL --exit-code 1 --ignore-unfixed --format json --output /work/$(DEMO_TARGET_TRIVY_REPORT_FILE) pulse-demo-target:ci
	docker run --rm -e SYFT_CHECK_FOR_APP_UPDATE=false -v /var/run/docker.sock:/var/run/docker.sock -v "$$(pwd)":/work $(SYFT_IMAGE) pulse:ci -o spdx-json=/work/$(SYFT_SBOM_FILE)
	docker run --rm -e SYFT_CHECK_FOR_APP_UPDATE=false -v /var/run/docker.sock:/var/run/docker.sock -v "$$(pwd)":/work $(SYFT_IMAGE) pulse-demo-target:ci -o spdx-json=/work/$(DEMO_TARGET_SYFT_SBOM_FILE)

supply-chain-check-laptop: ## Lower-CPU supply-chain check (audit only)
	@cargo audit --version 2>/dev/null | grep -Fq "$(CARGO_AUDIT_VERSION)" || cargo install cargo-audit --locked --version $(CARGO_AUDIT_VERSION) --force
	cargo audit
	cargo audit --file demo/grpc-target/Cargo.lock

ci-check-full: ci-check supply-chain-check ## Local equivalent of CI quality + supply-chain jobs

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

release-validate: ## Check crate, lockfile, changelog, tags, Docker, CI, and Rust versions agree
	VERSION=$(VERSION) scripts/release/validate-release.sh

release-tag: release-validate ## Create annotated semantic version tag (usage: make release-tag VERSION=0.1.3)
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required (example: make release-tag VERSION=0.1.3)"; exit 1; fi
	@if ! printf '%s\n' "$(VERSION)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$'; then echo "VERSION must be semantic x.y.z"; exit 1; fi
	@if [ -n "$$(git status --porcelain)" ]; then echo "working tree is not clean; commit or stash changes before tagging"; exit 1; fi
	@if git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null; then echo "tag v$(VERSION) already exists"; exit 1; fi
	git tag -a "v$(VERSION)" -m "Release v$(VERSION)"
	@echo "created tag v$(VERSION)"

release-tag-push: ## Push semantic version tag to origin (usage: make release-tag-push VERSION=0.1.3)
	@if [ -z "$(VERSION)" ]; then echo "VERSION is required (example: make release-tag-push VERSION=0.1.3)"; exit 1; fi
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
	@set -e; subnet="$$( $(DOCKER_SUBNET_SELECTOR) --network-name $(COMPOSE_NETWORK_NAME) )"; \
		PULSE_DOCKER_SUBNET="$$subnet" docker compose --project-name $(COMPOSE_PROJECT_NAME) up -d --build

docker-down: ## Stop pulse in Docker Compose
	@set -e; subnet="$$( $(DOCKER_SUBNET_SELECTOR) --network-name $(COMPOSE_NETWORK_NAME) )"; \
		PULSE_DOCKER_SUBNET="$$subnet" docker compose --project-name $(COMPOSE_PROJECT_NAME) down

docker-logs: ## Tail logs for pulse, prometheus, and grafana
	@set -e; subnet="$$( $(DOCKER_SUBNET_SELECTOR) --network-name $(COMPOSE_NETWORK_NAME) )"; \
		PULSE_DOCKER_SUBNET="$$subnet" docker compose --project-name $(COMPOSE_PROJECT_NAME) logs -f pulse prometheus grafana

test-compose-up: ## Start Kafka and Redis for docker-backed integration tests
	@set -e; subnet="$$( $(DOCKER_SUBNET_SELECTOR) --network-name $(COMPOSE_NETWORK_NAME) )"; \
		PULSE_DOCKER_SUBNET="$$subnet" docker compose --project-name $(COMPOSE_PROJECT_NAME) up -d --wait kafka redis

test-compose-down: ## Stop test dependencies
	@set -e; subnet="$$( $(DOCKER_SUBNET_SELECTOR) --network-name $(COMPOSE_NETWORK_NAME) )"; \
		PULSE_DOCKER_SUBNET="$$subnet" docker compose --project-name $(COMPOSE_PROJECT_NAME) down --remove-orphans

test-integration-compose: test-compose-up ## Run all ignored Kafka/Redis reliability tests against Compose
	PULSE_TEST_KAFKA_BROKERS=$(TEST_KAFKA_BROKERS) \
	PULSE_TEST_REDIS_URL=$(TEST_REDIS_URL) \
	$(CARGO) test --locked --test integration_compose -- --ignored --nocapture
	PULSE_TEST_REDIS_URL=$(TEST_REDIS_URL) \
	$(CARGO) test --locked --test redis_coordination -- --ignored --nocapture
	PULSE_TEST_REDIS_URL=$(TEST_REDIS_URL) \
	$(CARGO) test --locked --test redis_aggregation -- --ignored --nocapture

kind-build: ## Build the self-contained Pulse and target images for kind
	@mkdir -p $(KIND_INPUT_DIR)
	docker build -f demo/Dockerfile.pulse -t $(LOCAL_IMAGE) .
	docker build -f demo/grpc-target/Dockerfile -t $(DEMO_TARGET_IMAGE) .
	@set -eu; \
	descriptor_container="$$(docker create $(LOCAL_IMAGE))"; \
	trap 'docker rm "$$descriptor_container" >/dev/null 2>&1 || true' EXIT; \
	docker cp "$$descriptor_container":/app/descriptors/demo.pb $(KIND_INPUT_DIR)/demo.pb; \
	docker rm "$$descriptor_container" >/dev/null; \
	trap - EXIT

kind-pull-deps: ## Ensure dependency images exist locally before kind load
	docker image inspect $(KAFKA_IMAGE) >/dev/null 2>&1 || docker pull $(KAFKA_IMAGE)
	docker image inspect $(REDIS_IMAGE) >/dev/null 2>&1 || docker pull $(REDIS_IMAGE)
	docker image inspect $(PROMETHEUS_IMAGE) >/dev/null 2>&1 || docker pull $(PROMETHEUS_IMAGE)
	docker image inspect $(GRAFANA_IMAGE) >/dev/null 2>&1 || docker pull $(GRAFANA_IMAGE)

kind-load: ## Load the local Pulse and deterministic target images into kind
	kind load docker-image $(LOCAL_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(DEMO_TARGET_IMAGE) --name $(KIND_CLUSTER_NAME)

kind-load-deps: ## Load kafka/redis images into kind cluster
	$(MAKE) kind-pull-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) PROMETHEUS_IMAGE=$(PROMETHEUS_IMAGE) GRAFANA_IMAGE=$(GRAFANA_IMAGE)
	kind load docker-image $(KAFKA_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(REDIS_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(PROMETHEUS_IMAGE) --name $(KIND_CLUSTER_NAME)
	kind load docker-image $(GRAFANA_IMAGE) --name $(KIND_CLUSTER_NAME)

k8s-deploy-kind: ## Build + load the self-contained kind stack, then deploy it
	$(MAKE) kind-build LOCAL_IMAGE=$(LOCAL_IMAGE) DEMO_TARGET_IMAGE=$(DEMO_TARGET_IMAGE) KIND_INPUT_DIR=$(KIND_INPUT_DIR)
	$(MAKE) kind-load-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) PROMETHEUS_IMAGE=$(PROMETHEUS_IMAGE) GRAFANA_IMAGE=$(GRAFANA_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) kind-load LOCAL_IMAGE=$(LOCAL_IMAGE) DEMO_TARGET_IMAGE=$(DEMO_TARGET_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) k8s-deploy K8S_OVERLAY=kind
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout restart deployment/pulse deployment/grpc-target
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/grpc-target --timeout=$(K8S_ROLLOUT_TIMEOUT)
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/pulse --timeout=$(K8S_ROLLOUT_TIMEOUT)
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/kafka --timeout=$(K8S_ROLLOUT_TIMEOUT)
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/redis --timeout=$(K8S_ROLLOUT_TIMEOUT)
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/prometheus --timeout=$(K8S_ROLLOUT_TIMEOUT)
	kubectl --context $(K8S_KIND_CONTEXT) -n $(K8S_KIND_NAMESPACE) rollout status deployment/grafana --timeout=$(K8S_ROLLOUT_TIMEOUT)

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

k8s-guard-demo-stack: k8s-guard-overlay ## Require the kind overlay that owns demo dependencies
	@if [ "$(K8S_OVERLAY)" != "kind" ]; then \
		echo "K8S_OVERLAY=$(K8S_OVERLAY) is app-only; this command manages demo Kafka/Redis/observability and is limited to kind"; \
		exit 1; \
	fi

k8s-validate: ## Render the app base, demo package, overlays, and alert examples
	scripts/k8s/validate.sh

k8s-deploy: k8s-guard-overlay ## Deploy pulse to Kubernetes from selected overlay (K8S_OVERLAY=kind|staging|prod)
	kubectl --context $(KUBE_CONTEXT) create namespace $(KUBE_NAMESPACE) --dry-run=client -o yaml | kubectl --context $(KUBE_CONTEXT) apply -f -
	kubectl --context $(KUBE_CONTEXT) apply -k $(K8S_OVERLAY_DIR)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse --timeout=$(K8S_ROLLOUT_TIMEOUT)

k8s-deploy-push: ## Build + push image, then deploy (requires REGISTRY)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for k8s-deploy-push"; exit 1; fi
	$(MAKE) docker-build-image REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) docker-push REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) k8s-deploy KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE) K8S_OVERLAY=$(K8S_OVERLAY)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/pulse pulse=$(IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse --timeout=$(K8S_ROLLOUT_TIMEOUT)

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

k8s-leader-key: k8s-guard-demo-stack ## Show the leader key in the kind demo Redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/redis -- redis-cli HGETALL 'pulse:{coordination}:leader'

k8s-kafka-topics: k8s-guard-demo-stack ## List topics in the kind demo Kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list

k8s-pf-grafana: k8s-guard-demo-stack ## Port-forward the kind demo Grafana UI to localhost:3001
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

k8s-chaos-restart-kafka: k8s-guard-demo-stack ## Restart kind demo Kafka and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/kafka --timeout=300s

k8s-chaos-restart-redis: k8s-guard-demo-stack ## Restart kind demo Redis and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/redis --timeout=300s

k8s-chaos-restart-pulse: k8s-guard-overlay ## Restart Pulse deployment and wait for rollout
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout restart deployment/pulse
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse --timeout=300s

k8s-soak-chaos: k8s-guard-demo-stack ## Run the kind demo soak/chaos script
	KUBE_CONTEXT=$(KUBE_CONTEXT) \
	KUBE_NAMESPACE=$(KUBE_NAMESPACE) \
	SOAK_DURATION_SEC=$(SOAK_DURATION_SEC) \
	SOAK_SAMPLE_INTERVAL_SEC=$(SOAK_SAMPLE_INTERVAL_SEC) \
	SOAK_CHAOS_PLAN=$(SOAK_CHAOS_PLAN) \
	SOAK_REPORT_DIR=$(SOAK_REPORT_DIR) \
	SOAK_MIN_JOBS_RECEIVED=$(SOAK_MIN_JOBS_RECEIVED) \
	SOAK_MIN_RESULTS_PUBLISHED=$(SOAK_MIN_RESULTS_PUBLISHED) \
	SOAK_MIN_SOURCE_COMMITS=$(SOAK_MIN_SOURCE_COMMITS) \
	SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC=$(SOAK_POST_FAULT_ASSERT_TIMEOUT_SEC) \
	SOAK_POST_FAULT_POLL_INTERVAL_SEC=$(SOAK_POST_FAULT_POLL_INTERVAL_SEC) \
	SOAK_MIN_POST_FAULT_PROGRESS=$(SOAK_MIN_POST_FAULT_PROGRESS) \
	SOAK_EVIDENCE_ENABLED=$(SOAK_EVIDENCE_ENABLED) \
	SOAK_EVIDENCE_CLASS=$(SOAK_EVIDENCE_CLASS) \
	SOAK_EVIDENCE_DIR=$(SOAK_EVIDENCE_DIR) \
	SOAK_BUILD_PROFILE=$(SOAK_BUILD_PROFILE) \
	SOAK_SCENARIO_FILES=$(SOAK_SCENARIO_FILES) \
	SOAK_DESCRIPTOR_FILES=$(SOAK_DESCRIPTOR_FILES) \
	SOAK_TARGET_DEPLOYMENT=$(SOAK_TARGET_DEPLOYMENT) \
	SOAK_PULSE_CONFIGMAP=$(SOAK_PULSE_CONFIGMAP) \
	SOAK_TARGET_CONFIGMAP=$(SOAK_TARGET_CONFIGMAP) \
	bash scripts/reliability/soak_chaos.sh

k8s-check-performance: k8s-guard-demo-stack ## Evaluate kind demo smoke thresholds via its Prometheus
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
	PERF_EVIDENCE_ENABLED=$(PERF_EVIDENCE_ENABLED) \
	PERF_EVIDENCE_CLASS=$(PERF_EVIDENCE_CLASS) \
	PERF_EVIDENCE_DIR=$(PERF_EVIDENCE_DIR) \
	PERF_BUILD_PROFILE=$(PERF_BUILD_PROFILE) \
	PERF_SCENARIO_FILES=$(PERF_SCENARIO_FILES) \
	PERF_DESCRIPTOR_FILES=$(PERF_DESCRIPTOR_FILES) \
	PERF_TARGET_DEPLOYMENT=$(PERF_TARGET_DEPLOYMENT) \
	PERF_PULSE_CONFIGMAP=$(PERF_PULSE_CONFIGMAP) \
	PERF_TARGET_CONFIGMAP=$(PERF_TARGET_CONFIGMAP) \
	bash scripts/reliability/check_performance_thresholds.sh

k8s-fix-metrics-server: ## Patch kube-system metrics-server for kind kubelet TLS and verify metrics API
	kubectl --context $(KUBE_CONTEXT) -n kube-system patch deployment metrics-server --type='strategic' -p '{"spec":{"template":{"spec":{"containers":[{"name":"metrics-server","args":["--cert-dir=/tmp","--secure-port=10250","--kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP","--kubelet-use-node-status-port","--metric-resolution=15s","--kubelet-insecure-tls"]}]}}}}'
	kubectl --context $(KUBE_CONTEXT) -n kube-system rollout status deployment/metrics-server
	kubectl --context $(KUBE_CONTEXT) top nodes
