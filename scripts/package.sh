#!/usr/bin/env bash
#
# Build Tessera.app (a real macOS application bundle) from the Rust binary,
# generate its .icns icon from assets/icon.svg, ad-hoc code-sign it, and
# optionally wrap it in a .dmg.
#
# Usage:
#   scripts/package.sh           # build dist/Tessera.app
#   scripts/package.sh --dmg     # also build dist/Tessera.dmg
#
# Everything is produced under ./dist (git-ignored). Drag the .app to
# /Applications, then pin it to the Dock.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

APP_NAME="Tessera"
BIN_NAME="tessera"
BUNDLE_ID="com.elstarkov.tessera"
VERSION="$(sed -n 's/^version *= *"\(.*\)".*/\1/p' Cargo.toml | head -1)"

DIST="dist"
APP="$DIST/$APP_NAME.app"
ICONSET="$DIST/$APP_NAME.iconset"
SRC_PNG="$DIST/icon-1024.png"

rm -rf "$APP" "$ICONSET" "$DIST/$APP_NAME.icns" "$DIST/$APP_NAME.dmg"
mkdir -p "$DIST"

echo "==> 1/4  Icon  (assets/icon.svg -> $APP_NAME.icns)"
qlmanage -t -s 1024 -o "$DIST" assets/icon.svg >/dev/null 2>&1 || true
if [ -f "$DIST/icon.svg.png" ]; then
  sips -z 1024 1024 "$DIST/icon.svg.png" --out "$SRC_PNG" >/dev/null
  rm -f "$DIST/icon.svg.png"
elif [ ! -f "$SRC_PNG" ]; then
  echo "    ! could not rasterize the SVG; drop a 1024x1024 PNG at $SRC_PNG and re-run" >&2
  exit 1
fi
mkdir -p "$ICONSET"
gen() { sips -z "$1" "$1" "$SRC_PNG" --out "$ICONSET/$2" >/dev/null; }
gen 16   icon_16x16.png;     gen 32   icon_16x16@2x.png
gen 32   icon_32x32.png;     gen 64   icon_32x32@2x.png
gen 128  icon_128x128.png;   gen 256  icon_128x128@2x.png
gen 256  icon_256x256.png;   gen 512  icon_256x256@2x.png
gen 512  icon_512x512.png;   gen 1024 icon_512x512@2x.png
iconutil -c icns "$ICONSET" -o "$DIST/$APP_NAME.icns"
rm -rf "$ICONSET"

echo "==> 2/4  Build  (universal: arm64 + x86_64)"
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)
rustup target add "${TARGETS[@]}" >/dev/null 2>&1 || true
SLICES=()
for t in "${TARGETS[@]}"; do
  if cargo build --release --target "$t"; then
    SLICES+=("target/$t/release/$BIN_NAME")
  else
    echo "    ! build for $t failed (skipping that arch)"
  fi
done
if [ ${#SLICES[@]} -eq 0 ]; then
  echo "    falling back to host build"; cargo build --release
  SLICES=("target/release/$BIN_NAME")
fi
# Stitch the per-arch binaries into one universal binary (lipo is fine with 1).
lipo -create "${SLICES[@]}" -output "$DIST/$BIN_NAME-universal"

echo "==> 3/4  Assemble  $APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$DIST/$BIN_NAME-universal" "$APP/Contents/MacOS/$BIN_NAME"
cp "$DIST/$APP_NAME.icns" "$APP/Contents/Resources/$APP_NAME.icns"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundleExecutable</key><string>$BIN_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleIconFile</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "==> 4/4  Ad-hoc code-sign (lets it run locally; not for distribution)"
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || \
  echo "    ! ad-hoc sign failed (non-fatal)"
touch "$APP"  # nudge Finder to refresh the icon

if [ "${1:-}" = "--dmg" ]; then
  echo "==> +    Disk image  $DIST/$APP_NAME.dmg"
  hdiutil create -volname "$APP_NAME" -srcfolder "$APP" \
    -ov -format UDZO "$DIST/$APP_NAME.dmg" >/dev/null
fi

echo
echo "Done -> $APP"
echo "  Run it:        open '$APP'"
echo "  Install it:    cp -R '$APP' /Applications/   (then pin to Dock)"
