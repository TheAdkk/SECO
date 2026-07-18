#!/usr/bin/env bash
# Builds patada and assembles the macOS .clap bundle.
#
# CLAP on macOS is a *bundle* (a directory), not a bare dylib — hosts scan
# for bundles, and a renamed .dylib file will not be picked up. See
# reference/clap/include/clap/entry.h:22-24 for the search paths.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${1:-release}"
case "$PROFILE" in
  release) cargo build -p patada --release ;;
  debug)   cargo build -p patada ;;
  *) echo "usage: $0 [release|debug]" >&2; exit 1 ;;
esac

BUNDLE="target/patada.clap"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp "target/$PROFILE/libpatada.dylib" "$BUNDLE/Contents/MacOS/patada"
cat > "$BUNDLE/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>patada</string>
	<key>CFBundleIdentifier</key>
	<string>dev.seco.patada</string>
	<key>CFBundleName</key>
	<string>patada</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundleVersion</key>
	<string>0.1.0</string>
</dict>
</plist>
EOF

# Ad-hoc signature: unsigned code may be refused on Apple Silicon.
codesign --force --sign - "$BUNDLE"

echo "Built $BUNDLE"
echo "Install with: cp -R $BUNDLE ~/Library/Audio/Plug-Ins/CLAP/"
