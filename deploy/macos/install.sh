#!/bin/sh
# Builds HCP and installs it as a per-user launchd service on macOS.
#
#   deploy/macos/install.sh            build, install, and (re)start the service
#   deploy/macos/install.sh --no-load  build and install files only
#
# Installs:
#   ~/.local/bin/hcp                              the server binary
#   ~/.config/hcp/hcp.env                         configuration (created from the example
#                                                 if missing; never overwritten)
#   ~/Library/LaunchAgents/com.hcp.server.plist   the service definition (not on the first
#                                                 run, which stops for you to fill in the
#                                                 configuration)
set -eu

LABEL=com.hcp.server
REPO=$(cd "$(dirname "$0")/../.." && pwd)
BIN_DIR="$HOME/.local/bin"
CONFIG_DIR="$HOME/.config/hcp"
LOG_DIR="$HOME/Library/Logs/hcp"
AGENTS_DIR="$HOME/Library/LaunchAgents"
PLIST="$AGENTS_DIR/$LABEL.plist"

LOAD=1
[ "${1:-}" = "--no-load" ] && LOAD=0

# rustup installs cargo in ~/.cargo/bin and puts it on PATH from a shell profile, so a
# shell that has not read that profile cannot see it. Look there before giving up.
if ! command -v cargo >/dev/null 2>&1; then
  CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
  if [ -x "$CARGO_BIN/cargo" ]; then
    PATH="$CARGO_BIN:$PATH"
    export PATH
  else
    echo "cargo was not found on PATH or in $CARGO_BIN." >&2
    echo "Install Rust from https://rustup.rs, then run this script again." >&2
    exit 1
  fi
fi

echo "Building hcp (release)..."
cargo build --release --manifest-path "$REPO/Cargo.toml"

mkdir -p "$BIN_DIR" "$CONFIG_DIR" "$LOG_DIR" "$AGENTS_DIR"
install -m 755 "$REPO/target/release/hcp" "$BIN_DIR/hcp"

echo "Installed $BIN_DIR/hcp"

# Stop here on a first run. The service definition is only installed once there is a
# configuration for it: launchd loads everything in LaunchAgents at login, and a service
# started with the placeholder configuration would fail and be restarted indefinitely.
if [ ! -f "$CONFIG_DIR/hcp.env" ]; then
  install -m 600 "$REPO/deploy/macos/hcp.env.example" "$CONFIG_DIR/hcp.env"
  echo
  echo "Created $CONFIG_DIR/hcp.env from the example. Fill it in, then run this script again"
  echo "to install and start the service."
  exit 0
fi

sed "s|__HOME__|$HOME|g" "$REPO/deploy/macos/$LABEL.plist" > "$PLIST"
plutil -lint "$PLIST" >/dev/null
echo "Installed $PLIST"

if [ "$LOAD" = 1 ]; then
  DOMAIN="gui/$(id -u)"
  # Replace a running copy, if there is one.
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  launchctl bootstrap "$DOMAIN" "$PLIST"
  echo "Started $LABEL. Check it with:"
  echo "  curl http://127.0.0.1:3100/health"
  echo "  tail $LOG_DIR/stderr.log"
fi
