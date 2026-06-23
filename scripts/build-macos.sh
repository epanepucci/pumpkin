#!/usr/bin/env bash
# Build a macOS .app bundle and zip archive for Pumpkin.
#
# Usage:
#   scripts/build-macos.sh
#   scripts/build-macos.sh --target aarch64-apple-darwin
#
# Output:
#   builds/Pumpkin.app
#   builds/Pumpkin-macos-<arch-or-target>.zip

set -euo pipefail

APP_NAME="Pumpkin"
BINARY_NAME="pumpkin"
BUNDLE_ID="org.pumpkin.viewer"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$REPO_ROOT/builds"
APP_DIR="$BUILD_DIR/$APP_NAME.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"
ICONSET_DIR="$BUILD_DIR/$APP_NAME.iconset"
ICON_SOURCE="$REPO_ROOT/artwork/mrs-pumpkin-head.png"

TARGET=""
if [[ "${1:-}" == "--target" ]]; then
    if [[ -z "${2:-}" ]]; then
        echo "error: --target requires a Rust target triple" >&2
        exit 2
    fi
    TARGET="$2"
elif [[ $# -gt 0 ]]; then
    echo "usage: scripts/build-macos.sh [--target <rust-target-triple>]" >&2
    exit 2
fi

CARGO_ARGS=(build --release)
TARGET_DIR="$REPO_ROOT/target/release"
ARCH_LABEL="$(uname -m)"
if [[ -n "$TARGET" ]]; then
    CARGO_ARGS+=(--target "$TARGET")
    TARGET_DIR="$REPO_ROOT/target/$TARGET/release"
    ARCH_LABEL="$TARGET"
fi

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n 1)"
if [[ -z "$VERSION" ]]; then
    VERSION="0.0.0"
fi

echo "==> Building release binary..."
(cd "$REPO_ROOT" && cargo "${CARGO_ARGS[@]}")

echo "==> Creating app bundle..."
rm -rf "$APP_DIR" "$ICONSET_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"
cp "$TARGET_DIR/$BINARY_NAME" "$MACOS_DIR/$APP_NAME"
chmod +x "$MACOS_DIR/$APP_NAME"

ICON_PLIST=""
if command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1 && [[ -f "$ICON_SOURCE" ]]; then
    mkdir -p "$ICONSET_DIR"
    sips -z 16 16 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_16x16.png" >/dev/null
    sips -z 32 32 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_16x16@2x.png" >/dev/null
    sips -z 32 32 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_32x32.png" >/dev/null
    sips -z 64 64 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_32x32@2x.png" >/dev/null
    sips -z 128 128 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_128x128.png" >/dev/null
    sips -z 256 256 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_128x128@2x.png" >/dev/null
    sips -z 256 256 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_256x256.png" >/dev/null
    sips -z 512 512 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_256x256@2x.png" >/dev/null
    sips -z 512 512 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_512x512.png" >/dev/null
    sips -z 1024 1024 "$ICON_SOURCE" --out "$ICONSET_DIR/icon_512x512@2x.png" >/dev/null
    if iconutil -c icns "$ICONSET_DIR" -o "$RESOURCES_DIR/$APP_NAME.icns"; then
        ICON_PLIST="    <key>CFBundleIconFile</key>
    <string>$APP_NAME</string>"
    else
        echo "warning: iconutil rejected the generated iconset; bundle will use the default app icon" >&2
    fi
    rm -rf "$ICONSET_DIR"
else
    echo "warning: could not create .icns icon; sips/iconutil or source PNG missing" >&2
fi

cat > "$CONTENTS_DIR/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
$ICON_PLIST
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.education</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

if command -v codesign >/dev/null 2>&1; then
    echo "==> Applying ad-hoc signature..."
    if ! codesign --force --deep --sign - "$APP_DIR" >/dev/null 2>&1; then
        echo "warning: ad-hoc codesign failed; bundle was still created" >&2
    fi
fi

ZIP_NAME="$APP_NAME-macos-$ARCH_LABEL.zip"
echo "==> Creating zip archive..."
(
    cd "$BUILD_DIR"
    rm -f "$ZIP_NAME"
    if command -v ditto >/dev/null 2>&1; then
        ditto -c -k --sequesterRsrc --keepParent "$APP_NAME.app" "$ZIP_NAME"
    else
        zip -qry "$ZIP_NAME" "$APP_NAME.app"
    fi
)

echo ""
echo "Done:"
echo "  $APP_DIR"
echo "  $BUILD_DIR/$ZIP_NAME"
