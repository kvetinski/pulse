SHELL := /bin/bash

CARGO ?= cargo
PROTOC ?= protoc
RUST_LOG ?= info
KUBE_CONTEXT ?= kind-account
KUBE_NAMESPACE ?= pulse
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

.PHONY: help start start-release check fmt clippy bench ci-check proto-descriptor proto-descriptor-clean docker-build docker-build-image docker-push docker-rebuild docker-up docker-down docker-logs test-compose-up test-compose-down test-integration-compose kind-build kind-pull-deps kind-load kind-load-deps k8s-deploy-kind k8s-deploy k8s-deploy-push k8s-delete k8s-logs k8s-status k8s-leader-key k8s-kafka-topics k8s-pf-grafana k8s-apply-hpa-example k8s-apply-pdb-example k8s-fix-metrics-server

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

bench: ## Run benchmark binary (override env: PULSE_BENCH_* iterations)
	$(CARGO) run --release --bin pulse_bench

ci-check: ## Full local quality gates used in CI
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings
	$(CARGO) test --locked --all-targets --all-features
	PULSE_BENCH_TOKEN_BUCKET_ITERATIONS=200 PULSE_BENCH_RUNNER_ITERATIONS=5 $(CARGO) run --locked --release --bin pulse_bench
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

k8s-deploy-kind: ## Build + load local kind image, then deploy using it
	$(MAKE) kind-build LOCAL_IMAGE=$(LOCAL_IMAGE)
	$(MAKE) kind-load-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) PROMETHEUS_IMAGE=$(PROMETHEUS_IMAGE) GRAFANA_IMAGE=$(GRAFANA_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) kind-load LOCAL_IMAGE=$(LOCAL_IMAGE) KIND_CLUSTER_NAME=$(KIND_CLUSTER_NAME)
	$(MAKE) k8s-deploy KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE)

k8s-deploy: ## Deploy pulse to Kubernetes from k8s/kustomization.yaml
	kubectl --context $(KUBE_CONTEXT) create namespace $(KUBE_NAMESPACE) --dry-run=client -o yaml | kubectl --context $(KUBE_CONTEXT) apply -f -
	kubectl --context $(KUBE_CONTEXT) apply -k k8s
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/prometheus
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/grafana
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse

k8s-deploy-push: ## Build + push image, then deploy (requires REGISTRY)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for k8s-deploy-push"; exit 1; fi
	$(MAKE) docker-build-image REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) docker-push REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) k8s-deploy KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/pulse pulse=$(IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse

k8s-delete: ## Remove pulse resources from Kubernetes
	kubectl --context $(KUBE_CONTEXT) delete -k k8s --ignore-not-found=true

k8s-logs: ## Tail pulse pod logs from Kubernetes
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) logs -f deployment/pulse

k8s-status: ## Show pulse deployment and pod status
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) get deploy,pod -l app=pulse -o wide

k8s-leader-key: ## Show current redis leader key value (requires redis pod/service name 'redis')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/redis -- redis-cli GET pulse:leader

k8s-kafka-topics: ## List kafka topics (requires kafka deployment name 'kafka')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list

k8s-pf-grafana: ## Port-forward Grafana UI to localhost:3001
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) port-forward svc/grafana 3001:3000

k8s-apply-hpa-example: ## Apply sample HPA (requires metrics-server)
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/examples/hpa-pulse.yaml

k8s-apply-pdb-example: ## Apply sample stricter PDB (minAvailable=2)
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/examples/pdb-pulse.yaml

k8s-fix-metrics-server: ## Patch kube-system metrics-server for kind kubelet TLS and verify metrics API
	kubectl --context $(KUBE_CONTEXT) -n kube-system patch deployment metrics-server --type='strategic' -p '{"spec":{"template":{"spec":{"containers":[{"name":"metrics-server","args":["--cert-dir=/tmp","--secure-port=10250","--kubelet-preferred-address-types=InternalIP,Hostname,ExternalIP","--kubelet-use-node-status-port","--metric-resolution=15s","--kubelet-insecure-tls"]}]}}}}'
	kubectl --context $(KUBE_CONTEXT) -n kube-system rollout status deployment/metrics-server
	kubectl --context $(KUBE_CONTEXT) top nodes
