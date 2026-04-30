#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARY="$SCRIPT_DIR/pumpkin.rocky8"
CONFIG="$SCRIPT_DIR/config-${MAX_BEAMLINE}.toml"

exec "$BINARY" --config "$CONFIG" "$@"
