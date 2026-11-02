#!/usr/bin/env bash
# Baut "Claude Slots.app" als Universal-App (Apple Silicon + Intel) und packt
# sie als ZIP. Läuft nur auf einem Mac (braucht lipo, codesign und ditto aus
# den Xcode Command Line Tools: xcode-select --install).
#
#   scripts/macos-app.sh
#
# Ergebnis: target/macos/Claude Slots.app und target/macos/claude-slots-macos.zip
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
TARGETS=(aarch64-apple-darwin x86_64-apple-darwin)

rustup target add "${TARGETS[@]}"
for target in "${TARGETS[@]}"; do
  cargo build --release --target "$target"
done

OUT=target/macos
APP="$OUT/Claude Slots.app"
rm -rf "$APP" "$OUT/claude-slots-macos.zip"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

lipo -create -output "$APP/Contents/MacOS/claude-slots" \
  target/aarch64-apple-darwin/release/claude-slots \
  target/x86_64-apple-darwin/release/claude-slots

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>Claude Slots</string>
  <key>CFBundleDisplayName</key>
  <string>Claude Slots</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.mashen456.claude-slots</string>
  <key>CFBundleExecutable</key>
  <string>claude-slots</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${VERSION}</string>
  <key>CFBundleVersion</key>
  <string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key>
  <string>11.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
PLIST

# Ad-hoc-Signatur: ohne irgendeine Signatur startet macOS auf Apple Silicon
# nichts. Sie ersetzt keine Notarisierung, siehe README (Gatekeeper).
codesign --force --deep --sign - "$APP"

ditto -c -k --keepParent "$APP" "$OUT/claude-slots-macos.zip"
echo "Fertig: $APP"
echo "        $OUT/claude-slots-macos.zip"
