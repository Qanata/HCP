#!/bin/sh
# Builds the HCP desktop dashboard and installs it as ~/Applications/HCP.app.
#
#   deploy/macos/install-desktop.sh           build, install and open the app
#   deploy/macos/install-desktop.sh --no-open build and install only
#
# The app reads from the HCP server installed by deploy/macos/install.sh. On first launch
# it creates its own read-only admin key with `hcp keys create` and keeps it in the
# Keychain.
set -eu

REPO=$(cd "$(dirname "$0")/../.." && pwd)
APP="$HOME/Applications/HCP.app"
CRATE="$REPO/crates/hcp-desktop"

OPEN=1
[ "${1:-}" = "--no-open" ] && OPEN=0

if ! command -v cargo >/dev/null 2>&1; then
  CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
  if [ -x "$CARGO_BIN/cargo" ]; then
    PATH="$CARGO_BIN:$PATH"
    export PATH
  else
    echo "cargo was not found on PATH or in $CARGO_BIN. Install Rust from https://rustup.rs." >&2
    exit 1
  fi
fi

echo "Building HCP Desktop (release)..."
cargo build --release -p hcp-desktop --manifest-path "$REPO/Cargo.toml"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# The icon set macOS expects, from the 1024px master.
ICONSET="$WORK/icon.iconset"
mkdir "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size "$CRATE/icons/icon.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$CRATE/icons/icon.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$WORK/icon.icns"

# Assemble the bundle beside the old one, then swap, so a failed build leaves it intact.
STAGE="$WORK/HCP.app"
mkdir -p "$STAGE/Contents/MacOS" "$STAGE/Contents/Resources"
install -m 755 "$REPO/target/release/hcp-desktop" "$STAGE/Contents/MacOS/hcp-desktop"
cp "$WORK/icon.icns" "$STAGE/Contents/Resources/icon.icns"
cat > "$STAGE/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>HCP</string>
  <key>CFBundleDisplayName</key><string>HCP</string>
  <key>CFBundleIdentifier</key><string>com.hcp.desktop</string>
  <key>CFBundleExecutable</key><string>hcp-desktop</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
plutil -lint "$STAGE/Contents/Info.plist" >/dev/null
# An ad-hoc signature: enough for macOS to run it and for the Keychain to recognise it.
codesign --force --sign - "$STAGE" >/dev/null

mkdir -p "$HOME/Applications"
# Quit a running copy, and wait for it to exit, before replacing it: opening the new copy
# while the old one is still quitting fails (error -600).
if pgrep -f "$APP/Contents/MacOS/hcp-desktop" >/dev/null 2>&1; then
  osascript -e 'tell application id "com.hcp.desktop" to quit' >/dev/null 2>&1 || true
  i=0
  while pgrep -f "$APP/Contents/MacOS/hcp-desktop" >/dev/null 2>&1 && [ "$i" -lt 50 ]; do
    sleep 0.2
    i=$((i + 1))
  done
  pkill -f "$APP/Contents/MacOS/hcp-desktop" 2>/dev/null || true
fi
rm -rf "$APP"
mv "$STAGE" "$APP"
echo "Installed $APP"

if [ "$OPEN" = 1 ]; then
  open "$APP"
fi
