#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

fail() {
  echo "release validation failed: $*" >&2
  exit 1
}

cargo_version="$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ && in_package { exit }
  in_package && /^version = / { gsub(/[\"[:space:]]/, "", $3); print $3; exit }
' Cargo.toml)"

[[ "$cargo_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || fail "Cargo.toml package version is not semantic: '$cargo_version'"

requested_version="${VERSION:-$cargo_version}"
[[ "$requested_version" == "$cargo_version" ]] \
  || fail "VERSION=$requested_version does not match Cargo.toml version $cargo_version"

lock_version="$(awk '
  /^name = "pulse"$/ { pulse = 1; next }
  pulse && /^version = / { gsub(/[\"[:space:]]/, "", $3); print $3; exit }
' Cargo.lock)"
[[ "$lock_version" == "$cargo_version" ]] \
  || fail "Cargo.lock pulse version $lock_version does not match Cargo.toml $cargo_version"

grep -Fq "## [$cargo_version]" CHANGELOG.md \
  || fail "CHANGELOG.md has no [$cargo_version] release section"

toolchain="$(awk -F '"' '/^channel = / { print $2; exit }' rust-toolchain.toml)"
[[ -n "$toolchain" ]] || fail "rust-toolchain.toml has no channel"
cargo_rust_version="$(awk '
  /^\[package\]$/ { in_package = 1; next }
  /^\[/ && in_package { exit }
  in_package && /^rust-version = / { gsub(/["[:space:]]/, "", $3); print $3; exit }
' Cargo.toml)"
[[ "$cargo_rust_version" == "${toolchain%.*}" ]] \
  || fail "Cargo.toml rust-version $cargo_rust_version does not match Rust toolchain $toolchain"
for dockerfile in Dockerfile demo/Dockerfile.pulse demo/grpc-target/Dockerfile; do
  grep -Eq "^FROM rust:${toolchain}(-|@)" "$dockerfile" \
    || fail "$dockerfile builder does not use Rust $toolchain"
done

default_image_tag="$(awk '/^IMAGE_TAG \?= / { print $3; exit }' Makefile)"
[[ "$default_image_tag" == "$cargo_version" ]] \
  || fail "Makefile IMAGE_TAG=$default_image_tag does not match Cargo.toml $cargo_version"

grep -Fq "image: pulse:${cargo_version}" k8s/base/deployment.yaml \
  || fail "k8s/base/deployment.yaml image does not match Cargo.toml $cargo_version"
for overlay in staging prod; do
  grep -Fq "newTag: \"${cargo_version}\"" "k8s/overlays/${overlay}/kustomization.yaml" \
    || fail "k8s ${overlay} image tag does not match Cargo.toml $cargo_version"
done

if grep -REn --include='*.yml' --include='*.yaml' \
  'rust-toolchain@(stable|beta|nightly)|toolchain:[[:space:]]*(stable|beta|nightly)' \
  .github/workflows; then
  fail "CI contains a floating Rust toolchain; rely on rust-toolchain.toml"
fi

invalid_tags="$(git tag --list | grep -Ev '^v[0-9]+\.[0-9]+\.[0-9]+$' || true)"
[[ -z "$invalid_tags" ]] \
  || fail "non-semantic release tag(s): $(tr '\n' ' ' <<<"$invalid_tags")"

latest_tag="$(git tag --list 'v[0-9]*' --sort=-version:refname | head -n 1)"
if [[ -n "$latest_tag" ]]; then
  latest_version="${latest_tag#v}"
  highest="$(printf '%s\n%s\n' "$latest_version" "$cargo_version" | sort -V | tail -n 1)"
  [[ "$highest" == "$cargo_version" ]] \
    || fail "Cargo.toml version $cargo_version is older than latest tag $latest_tag"
  if [[ "$latest_version" == "$cargo_version" ]]; then
    tag_commit="$(git rev-parse "${latest_tag}^{}")"
    head_commit="$(git rev-parse HEAD)"
    [[ "$tag_commit" == "$head_commit" ]] \
      || fail "Cargo.toml reuses $latest_tag away from its tagged commit; advance the version before release"
  fi
fi

cargo metadata --locked --no-deps --format-version 1 >/dev/null
echo "release metadata is consistent for Pulse $cargo_version (Rust $toolchain)"
