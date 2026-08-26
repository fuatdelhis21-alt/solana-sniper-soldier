#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "Usage: $0 <artifact.tar.gz> [optional-config.toml]"
  exit 2
fi

ARTIFACT="$1"
OPTIONAL_CONFIG="$2"
RELEASE_DIR=/opt/solana-hft
BIN_DIR="$RELEASE_DIR/bin"
CONFIG_DIR=/etc/solana-hft
SERVICE=solana-hft

echo "Preparing release: $ARTIFACT"

id -u solana >/dev/null 2>&1 || useradd -r -s /sbin/nologin solana
mkdir -p "$BIN_DIR" "$CONFIG_DIR"

TMPDIR=$(mktemp -d)
tar -xzf "$ARTIFACT" -C "$TMPDIR"

echo "Installing binaries to $BIN_DIR"
cp -r "$TMPDIR"/* "$BIN_DIR"/
rm -rf "$TMPDIR"

if [ -n "${OPTIONAL_CONFIG:-}" ] && [ -f "$OPTIONAL_CONFIG" ]; then
  echo "Installing config to $CONFIG_DIR/config.toml"
  cp "$OPTIONAL_CONFIG" "$CONFIG_DIR/config.toml"
fi

chown -R solana:solana "$RELEASE_DIR" "$CONFIG_DIR"
chmod -R 0755 "$BIN_DIR"

echo "Reloading systemd and starting service"
systemctl daemon-reload
systemctl restart "$SERVICE"
systemctl enable "$SERVICE"

echo "Install complete"
