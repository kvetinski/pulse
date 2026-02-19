SHELL := /bin/bash

CARGO ?= cargo
RUST_LOG ?= info
KUBE_CONTEXT ?= kind-account
KUBE_NAMESPACE ?= account
KIND_CLUSTER ?= account
LOCAL_IMAGE ?= pulse:local
KAFKA_IMAGE ?= apache/kafka:3.9.0
REDIS_IMAGE ?= redis:7-alpine
REGISTRY ?=
IMAGE_REPO ?= pulse
IMAGE_TAG ?= latest
IMAGE ?= $(if $(REGISTRY),$(REGISTRY)/$(IMAGE_REPO):$(IMAGE_TAG),$(IMAGE_REPO):$(IMAGE_TAG))

.PHONY: help start start-release check fmt clippy docker-build docker-build-image docker-push docker-rebuild docker-up docker-down docker-logs kind-build kind-load kind-load-deps k8s-deploy-kind k8s-deploy k8s-deploy-push k8s-delete k8s-logs k8s-status k8s-leader-key k8s-kafka-topics

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

docker-build: ## Build Docker image via compose
	docker compose build pulse

docker-build-image: ## Build app image tag used for k8s (IMAGE/REGISTRY/IMAGE_REPO/IMAGE_TAG)
	docker build -t $(IMAGE) .

docker-push: ## Push IMAGE to registry (set REGISTRY, e.g. ghcr.io/org)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for docker-push"; exit 1; fi
	docker push $(IMAGE)

docker-rebuild: ## Rebuild Docker image without cache
	docker compose build --no-cache pulse

docker-up: ## Start pulse in Docker Compose
	docker compose up -d pulse

docker-down: ## Stop pulse in Docker Compose
	docker compose down

docker-logs: ## Tail pulse container logs
	docker compose logs -f pulse

kind-build: ## Build local image for kind (LOCAL_IMAGE=pulse:local)
	docker build -t $(LOCAL_IMAGE) .

kind-load: ## Load local image into kind cluster
	kind load docker-image $(LOCAL_IMAGE) --name $(KIND_CLUSTER)

kind-load-deps: ## Load kafka/redis images into kind cluster
	kind load docker-image $(KAFKA_IMAGE) --name $(KIND_CLUSTER)
	kind load docker-image $(REDIS_IMAGE) --name $(KIND_CLUSTER)

k8s-deploy-kind: ## Build + load local kind image, then deploy using it
	$(MAKE) kind-build LOCAL_IMAGE=$(LOCAL_IMAGE)
	$(MAKE) kind-load-deps KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) KIND_CLUSTER=$(KIND_CLUSTER)
	$(MAKE) kind-load LOCAL_IMAGE=$(LOCAL_IMAGE) KIND_CLUSTER=$(KIND_CLUSTER)
	$(MAKE) k8s-deploy IMAGE=$(LOCAL_IMAGE) KAFKA_IMAGE=$(KAFKA_IMAGE) REDIS_IMAGE=$(REDIS_IMAGE) KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE)

k8s-deploy: ## Deploy pulse to Kubernetes (context=account by default)
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/namespace.yaml
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/redis.yaml
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/kafka.yaml
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/configmap.yaml
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/pdb.yaml
	kubectl --context $(KUBE_CONTEXT) apply -f k8s/deployment.yaml
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/redis redis=$(REDIS_IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/kafka kafka=$(KAFKA_IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/redis
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/kafka
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) set image deployment/pulse pulse=$(IMAGE)
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) rollout status deployment/pulse

k8s-deploy-push: ## Build + push image, then deploy (requires REGISTRY)
	@if [ -z "$(REGISTRY)" ]; then echo "REGISTRY is required for k8s-deploy-push"; exit 1; fi
	$(MAKE) docker-build-image REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) docker-push REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG)
	$(MAKE) k8s-deploy REGISTRY=$(REGISTRY) IMAGE_REPO=$(IMAGE_REPO) IMAGE_TAG=$(IMAGE_TAG) KUBE_CONTEXT=$(KUBE_CONTEXT) KUBE_NAMESPACE=$(KUBE_NAMESPACE)

k8s-delete: ## Remove pulse resources from Kubernetes
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete deployment pulse --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete deployment kafka --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete deployment redis --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete service kafka --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete service redis --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete pdb pulse --ignore-not-found
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) delete configmap pulse-config --ignore-not-found

k8s-logs: ## Tail pulse pod logs from Kubernetes
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) logs -f deployment/pulse

k8s-status: ## Show pulse deployment and pod status
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) get deploy,pod -l app=pulse -o wide

k8s-leader-key: ## Show current redis leader key value (requires redis pod/service name 'redis')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/redis -- redis-cli GET pulse:leader

k8s-kafka-topics: ## List kafka topics (requires kafka deployment name 'kafka')
	kubectl --context $(KUBE_CONTEXT) -n $(KUBE_NAMESPACE) exec deploy/kafka -- /opt/kafka/bin/kafka-topics.sh --bootstrap-server kafka:9092 --list
