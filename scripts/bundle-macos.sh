#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
mode="${1:-release}"
case "$mode" in
  debug) cargo build -p dotdir ;;
  release) cargo build -p dotdir --release ;;
  *) printf '%s\n' 'Usage: scripts/bundle-macos.sh [debug|release]' >&2; exit 1 ;;
esac
bundle="dist/DotDir Native.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources/extensions"
cp "target/$mode/dotdir" "$bundle/Contents/MacOS/dotdir.new"
mv "$bundle/Contents/MacOS/dotdir.new" "$bundle/Contents/MacOS/dotdir"
for extension in extensions/*; do
  if [ -f "$extension/extension.json" ]; then
    cp -R "$extension" "$bundle/Contents/Resources/extensions/"
  fi
done
cp assets/icon.icns "$bundle/Contents/Resources/icon.icns"
cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>DotDir Native</string>
<key>CFBundleDisplayName</key><string>DotDir Native</string>
<key>CFBundleExecutable</key><string>dotdir</string>
<key>CFBundleIdentifier</key><string>dev.dotdir.native</string>
<key>CFBundleVersion</key><string>0.1.0</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleIconFile</key><string>icon.icns</string>
<key>NSHighResolutionCapable</key><true/>
<key>LSMinimumSystemVersion</key><string>12.0</string>
<key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict></plist>
PLIST
codesign --force --deep --sign - "$bundle"
printf '%s\n' "$PWD/$bundle"
