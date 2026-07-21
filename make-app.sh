#!/bin/sh
# Builds the release binary and wraps it in a macOS .app bundle, so
# double-clicking launches the GUI without a Terminal window.
# (On Windows this is not needed: the exe already hides its console.)
set -eu
cd "$(dirname "$0")"

cargo build --release

APP="target/release/WWM Jungle Timer.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/wwmts3gw "$APP/Contents/MacOS/wwmts3gw"

cat > "$APP/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>wwmts3gw</string>
	<key>CFBundleIdentifier</key>
	<string>net.strobl.wwmts3gw</string>
	<key>CFBundleName</key>
	<string>WWM Jungle Timer</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
EOF

echo "Created: $APP"
