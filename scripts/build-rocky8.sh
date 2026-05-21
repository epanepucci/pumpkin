#!/usr/bin/env bash
# Build pumpkin for Rocky Linux 8 inside a Docker container.
# The resulting binary is compatible with glibc 2.28 and the system
# libraries available on Rocky Linux 8.10.
#
# The Dockerfile uses four stages so that expensive layers are cached and
# reused across builds:
#
#   pumpkin-rocky8-base  — Rocky Linux 8 + Rust toolchain
#                          Rebuilt only when the package list or rustup changes.
#
#   pumpkin-rocky8-deps  — All Cargo dependencies compiled (includes HDF5 cmake).
#                          Rebuilt only when Cargo.toml or Cargo.lock changes.
#
#   (builder)            — Application crate only; fast on every source change.
#
#   (export/scratch)     — Single-file image used with --output to copy the
#                          binary directly to the host, bypassing the multi-GB
#                          target/ layer serialisation that made extraction slow.
#
# Usage:
#   scripts/build-rocky8.sh [--no-cache]
#
# Output:
#   ./pumpkin-rocky8   — the release binary, ready to scp to the target host

set -euo pipefail

IMAGE="pumpkin-rocky8-builder"
OUTPUT="pumpkin-rocky8"
EXTRACT_DIR="$(mktemp -d)"

DOCKER_FLAGS=""
if [[ "${1:-}" == "--no-cache" ]]; then
    DOCKER_FLAGS="--no-cache"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Build each stage with an explicit tag.  Named images are not removed by
# "docker system prune", so the cache survives routine cleanup.
echo "==> Stage 1/3: base image (Rocky Linux 8 + Rust toolchain)…"
docker build $DOCKER_FLAGS \
    --target base \
    -t "${IMAGE}-base" \
    -f "$REPO_ROOT/Dockerfile" \
    "$REPO_ROOT"

echo "==> Stage 2/3: deps image (all Cargo dependencies + HDF5)…"
docker build $DOCKER_FLAGS \
    --target deps \
    -t "${IMAGE}-deps" \
    -f "$REPO_ROOT/Dockerfile" \
    "$REPO_ROOT"

# Build the export stage and write the binary directly to the host.
# --output skips Docker image layer serialisation entirely — the scratch image
# contains only the binary, so there is no multi-GB target/ tree to compress.
echo "==> Stage 3/3: application build + binary extraction…"
docker build $DOCKER_FLAGS \
    --target export \
    --output "type=local,dest=$EXTRACT_DIR" \
    -f "$REPO_ROOT/Dockerfile" \
    "$REPO_ROOT"

cp "$EXTRACT_DIR/pumpkin" "$REPO_ROOT/$OUTPUT"
rm -rf "$EXTRACT_DIR"

chmod +x "$REPO_ROOT/$OUTPUT"
echo ""
echo "Done: $REPO_ROOT/$OUTPUT"
echo ""
echo "Runtime dependencies on the target Rocky Linux 8 machine:"
echo "  dnf install -y epel-release"
echo "  dnf install -y hdf5 libxkbcommon gtk3"
echo "  dnf install -y vulkan-loader          # wgpu GPU rendering"
echo "  dnf install -y mesa-vulkan-drivers    # software Vulkan fallback (no GPU)"
