#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "Building release binaries (resguard + rs-dash)..."
cargo build --release --workspace

APP_NAME="RS Dash"
APP_DIR="$ROOT/target/release/${APP_NAME}.app"
CONTENTS="$APP_DIR/Contents"
MACOS_DIR="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES"

cp "$ROOT/target/release/rs-dash" "$MACOS_DIR/rs-dash"
chmod +x "$MACOS_DIR/rs-dash"

# Generate AppIcon.icns using rs-dash's built-in vector-to-RGBA renderer + sips + iconutil
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

ICONSET="$TMP_DIR/AppIcon.iconset"
mkdir -p "$ICONSET"
"$MACOS_DIR/rs-dash" --export-icon-tga "$TMP_DIR/icon_512.tga"

if command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    sips -s format png "$TMP_DIR/icon_512.tga" --out "$ICONSET/icon_512x512.png" >/dev/null
    sips -z 16 16     "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_16x16.png" >/dev/null
    sips -z 32 32     "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_16x16@2x.png" >/dev/null
    sips -z 32 32     "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_32x32.png" >/dev/null
    sips -z 64 64     "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_32x32@2x.png" >/dev/null
    sips -z 128 128   "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_128x128.png" >/dev/null
    sips -z 256 256   "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_128x128@2x.png" >/dev/null
    sips -z 256 256   "$ICONSET/icon_512x512.png" --out "$ICONSET/icon_256x256.png" >/dev/null
    cp "$ICONSET/icon_512x512.png" "$ICONSET/icon_256x256@2x.png"
    iconutil -c icns "$ICONSET" -o "$RESOURCES/AppIcon.icns"
fi

cat > "$CONTENTS/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>rs-dash</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.resguard.rs-dash</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>RS Dash</string>
    <key>CFBundleDisplayName</key>
    <string>RS Dash — Resguard Dashboard</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

echo "Built macOS app bundle: $APP_DIR"
echo "Run with: open \"$APP_DIR\""
