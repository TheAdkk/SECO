#!/usr/bin/env bash
# Builds patada and assembles the macOS .clap bundle.
#
# CLAP on macOS is a *bundle* (a directory), not a bare dylib — hosts scan
# for bundles, and a renamed .dylib file will not be picked up. See
# reference/clap/include/clap/entry.h:22-24 for the search paths.
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-release}"
case "$MODE" in
  release) cargo build -p patada --release; PROFILE=release; EXT=clap ;;
  debug)   cargo build -p patada;           PROFILE=debug;   EXT=clap ;;
  # VST3 via clap-wrapper (C++ inside, embedded MIT VST3 SDK): the same
  # dylib carries clap_entry plus the VST3 entry points; only the bundle
  # extension tells hosts which face to load. Debug profile: keeps the
  # transport trace during the verification phase.
  vst3)    cargo build -p patada --features vst3; PROFILE=debug; EXT=vst3 ;;
  *) echo "usage: $0 [release|debug|vst3]" >&2; exit 1 ;;
esac

BUNDLE="target/patada.$EXT"
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
if [ "$EXT" = "vst3" ]; then
  echo "Install with: cp -R $BUNDLE ~/Library/Audio/Plug-Ins/VST3/"
else
  echo "Install with: cp -R $BUNDLE ~/Library/Audio/Plug-Ins/CLAP/"
fi
