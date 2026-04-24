#!/usr/bin/env bash
# Build pumpkin for Rocky Linux 8 inside a Docker container.
# The resulting binary is compatible with glibc 2.28 and the system
# libraries available on Rocky Linux 8.10.
#
# Usage:
#   scripts/build-rocky8.sh [--no-cache]
#
# Output:
#   ./pumpkin-rocky8   — the release binary, ready to scp to the target host

set -euo pipefail

IMAGE="pumpkin-rocky8-builder"
BINARY="pumpkin"
OUTPUT="pumpkin-rocky8"
CONTAINER="pumpkin-rocky8-extract-$$"

DOCKER_FLAGS=""
if [[ "${1:-}" == "--no-cache" ]]; then
    DOCKER_FLAGS="--no-cache"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

echo "==> Building Docker image (Rocky Linux 8)…"
docker build $DOCKER_FLAGS -t "$IMAGE" -f "$REPO_ROOT/Dockerfile" "$REPO_ROOT"

echo "==> Extracting binary…"
docker create --name "$CONTAINER" "$IMAGE" /bin/true
docker cp "$CONTAINER:/src/target/release/$BINARY" "$REPO_ROOT/$OUTPUT"
docker rm "$CONTAINER"

chmod +x "$REPO_ROOT/$OUTPUT"
echo ""
echo "Done: $REPO_ROOT/$OUTPUT"
echo ""
echo "Runtime dependencies on the target Rocky Linux 8 machine:"
echo "  dnf install -y epel-release"
echo "  dnf install -y hdf5 libxkbcommon gtk3"
echo "  dnf install -y vulkan-loader          # wgpu GPU rendering"
echo "  dnf install -y mesa-vulkan-drivers    # software Vulkan fallback (no GPU)"
