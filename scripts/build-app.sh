#!/bin/bash
# Builds dist/Lanes.app (release, ad-hoc signed).
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
cargo build --release

APP=dist/Lanes.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/lanes "$APP/Contents/MacOS/Lanes"

# Icon
if [ ! -f assets/icon.png ]; then swift scripts/make-icon.swift assets/icon.png; fi
ICONSET=$(mktemp -d)/Lanes.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z $s $s assets/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  sips -z $((s*2)) $((s*2)) assets/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Lanes.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Lanes</string>
  <key>CFBundleDisplayName</key><string>Lanes</string>
  <key>CFBundleIdentifier</key><string>app.lanes.Lanes</string>
  <key>CFBundleExecutable</key><string>Lanes</string>
  <key>CFBundleIconFile</key><string>Lanes</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSHumanReadableCopyright</key><string>Lanes</string>
</dict>
</plist>
PLIST

# Sign with the personal certificate (scripts/setup-signing.sh) so macOS keeps
# the Accessibility permission across rebuilds. Falls back to ad-hoc signing.
IDENTITY=$(security find-certificate -c "Lanes Local Signing" -Z 2>/dev/null | awk '/SHA-1/ {print $3}')
if [ -n "$IDENTITY" ]; then
  codesign --force --deep --sign "$IDENTITY" --identifier app.lanes.Lanes "$APP"
else
  echo "warning: no \"Lanes Local Signing\" certificate; run scripts/setup-signing.sh to stop re-granting permissions"
  codesign --force --deep --sign - --identifier app.lanes.Lanes "$APP"
fi
echo "Built $APP ($(du -sh "$APP" | cut -f1), binary $(du -h "$APP/Contents/MacOS/Lanes" | cut -f1))"
