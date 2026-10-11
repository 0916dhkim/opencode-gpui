#!/usr/bin/env bash
# Build from this checkout, but keep Cargo's target on a Linux-native Docker
# volume. Docker Desktop's macOS bind mount rounds explicit mtimes to seconds,
# which otherwise makes Cargo rebuild dependencies after a small source edit.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
checkout_id="$(printf '%s' "$root" | cksum | awk '{print $1}')"
target_volume="${OPENCODE_GPUI_TARGET_VOLUME:-opencode-gpui-target-${checkout_id}}"
registry_volume="${OPENCODE_GPUI_REGISTRY_VOLUME:-opencode-gpui-registry}"
jobs="${CARGO_BUILD_JOBS:-2}"

if [[ ! "$jobs" =~ ^[1-9][0-9]*$ ]]; then
    printf 'CARGO_BUILD_JOBS must be a positive integer\n' >&2
    exit 2
fi

printf 'Building %s with target volume %s and registry volume %s (%s jobs)\n' \
    "$root" "$target_volume" "$registry_volume" "$jobs"

docker run --rm --platform linux/amd64 \
    --mount "type=bind,source=${root},target=/app" \
    --mount "type=volume,source=${target_volume},target=/cargo-target" \
    --mount "type=volume,source=${registry_volume},target=/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/cargo-target \
    -e CARGO_BUILD_JOBS="$jobs" \
    -w /app opencode-gpui-builder-amd64:latest \
    bash -ceu '
        cargo build --locked --release
        mkdir -p /app/target/release
        install -m 755 /cargo-target/release/opencode-gpui /app/target/release/opencode-gpui
    '

printf 'Built %s/target/release/opencode-gpui\n' "$root"
