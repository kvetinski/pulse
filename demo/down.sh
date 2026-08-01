#!/usr/bin/env bash
set -euo pipefail

DEMO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${DEMO_DIR}/.." && pwd)"
PULSE_DOCKER_SUBNET="${PULSE_DOCKER_SUBNET:-$(
    "${REPO_ROOT}/scripts/docker/select_subnet.py" \
        --network-name pulse-demo_default
)}"
export PULSE_DOCKER_SUBNET
docker compose --project-name pulse-demo --file "${DEMO_DIR}/compose.yaml" \
    down --volumes --remove-orphans
