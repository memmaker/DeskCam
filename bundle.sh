#!/bin/sh
# Builds DeskCam.app (release) next to this script.
set -e
cd "$(dirname "$0")"
cargo build --release
APP=DeskCam.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/deskcam "$APP/Contents/MacOS/DeskCam"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>DeskCam</string>
  <key>CFBundleIdentifier</key><string>com.1kloc.deskcam</string>
  <key>CFBundleExecutable</key><string>DeskCam</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>DeskCam records your voice into screen recordings.</string>
  <key>NSScreenCaptureUsageDescription</key><string>DeskCam records your screen.</string>
</dict></plist>
PLIST
# Signing identity: set DESKCAM_SIGN to a stable certificate so macOS keeps the
# Screen Recording / Microphone grants across rebuilds; ad-hoc otherwise.
codesign --force --sign "${DESKCAM_SIGN:--}" "$APP"
echo "Built $(pwd)/$APP"
