#!/usr/bin/env bash
# Builds medatat.app from the release binary. macOS half of M8.
#
# Signing and notarisation are deliberately NOT here: they need an Apple Developer
# identity this project does not have, and a script that silently produces an unsigned
# bundle while looking like it signed one is worse than a script that says it cannot.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/medatat-ui"
APP="$ROOT/dist/medatat.app"
VERSION="$(grep -m1 '^version' "$ROOT/crates/medatat-ui/Cargo.toml" | cut -d'"' -f2)"

[ -x "$BIN" ] || { echo "error: $BIN not built. Run: cargo build --release -p medatat-ui" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/medatat"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>medatat</string>
  <key>CFBundleDisplayName</key><string>medatat</string>
  <key>CFBundleIdentifier</key><string>email.cetify.medatat</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>medatat</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <!-- The app is a data-entry tool with no document types and no URL schemes; it
       deliberately declares neither, so macOS never offers it as a handler for anything. -->
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "built $APP ($(du -sh "$APP" | cut -f1))"

if codesign --verify --deep --strict "$APP" 2>/dev/null; then
  echo "signed: yes"
else
  echo "signed: NO — needs an Apple Developer identity."
  echo "  Gatekeeper will refuse this on any machine but the one that built it."
  echo "  To sign:   codesign --deep --force --options runtime --sign \"Developer ID Application: NAME\" $APP"
  echo "  To notarise: xcrun notarytool submit ... && xcrun stapler staple $APP"
fi
