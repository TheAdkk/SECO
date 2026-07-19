#!/usr/bin/env bash
# Builds zape and assembles the macOS .clap bundle.
#
# CLAP on macOS is a *bundle* (a directory), not a bare dylib — hosts scan
# for bundles, and a renamed .dylib file will not be picked up. See
# reference/clap/include/clap/entry.h:22-24 for the search paths.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-release}"
case "$MODE" in
  release) cargo build -p zape --release; PROFILE=release; EXT=clap ;;
  debug)   cargo build -p zape;           PROFILE=debug;   EXT=clap ;;
  # VST3 via clap-wrapper (C++ inside, embedded MIT VST3 SDK): the same
  # dylib carries clap_entry plus the VST3 entry points; only the bundle
  # extension tells hosts which face to load. Debug profile: keeps the
  # transport trace during the verification phase.
  vst3)    cargo build -p zape --features vst3; PROFILE=debug; EXT=vst3 ;;
  # Editor builds (WKWebView behind the gui feature, macOS only).
  gui)     cargo build -p zape --features gui;          PROFILE=debug; EXT=clap ;;
  vst3-gui) cargo build -p zape --features "vst3 gui";  PROFILE=debug; EXT=vst3 ;;
  *) echo "usage: $0 [release|debug|vst3|gui|vst3-gui]" >&2; exit 1 ;;
esac

BUNDLE="target/zape.$EXT"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS"
cp "target/$PROFILE/libzape.dylib" "$BUNDLE/Contents/MacOS/zape"
cat > "$BUNDLE/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>zape</string>
	<key>CFBundleIdentifier</key>
	<string>dev.seco.zape</string>
	<key>CFBundleName</key>
	<string>Zape</string>
	<key>CFBundlePackageType</key>
	<string>BNDL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.4.0</string>
	<key>CFBundleVersion</key>
	<string>0.4.0</string>
</dict>
</plist>
EOF

# Ad-hoc signature: unsigned code may be refused on Apple Silicon.
codesign --force --sign - "$BUNDLE"

echo "Built $BUNDLE"
if [ "$EXT" = "vst3" ]; then
  echo "Install with: cp -R $BUNDLE ~/Library/Audio/Plug-Ins/VST3/"
else
  echo "Install with: cp -R $BUNDLE ~/Library/Audio/Plug-Ins/CLAP/"
fi
