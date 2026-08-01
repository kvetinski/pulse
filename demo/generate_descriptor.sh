#!/usr/bin/env bash
set -euo pipefail

DEMO_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
OUTPUT="${1:-${DEMO_DIR}/generated/demo.pb}"
OUTPUT_DIR="$(dirname -- "${OUTPUT}")"
mkdir -p -- "${OUTPUT_DIR}"

TEMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "${TEMP_DIR}"' EXIT
protoc \
    -I "${DEMO_DIR}/grpc-target/proto" \
    --include_imports \
    --descriptor_set_out="${TEMP_DIR}/demo.pb" \
    "${DEMO_DIR}/grpc-target/proto/demo.proto"
mv -- "${TEMP_DIR}/demo.pb" "${OUTPUT}"
echo "descriptor written to ${OUTPUT}"
