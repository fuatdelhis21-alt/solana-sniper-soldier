#!/usr/bin/env bash
# Roll back to the binaries that were installed before the last
# install_release.sh run (/opt/solana-hft/bin.prev) and restart.
# Bot state (/var/lib/solana-hft: risk state, open position) is untouched.
set -euo pipefail
RELEASE_DIR=/opt/solana-hft
if [ ! -d "$RELEASE_DIR/bin.prev" ]; then
  echo "no previous release in $RELEASE_DIR/bin.prev" >&2
  exit 1
fi
systemctl stop solana-hft.service
mv "$RELEASE_DIR/bin" "$RELEASE_DIR/bin.failed"
mv "$RELEASE_DIR/bin.prev" "$RELEASE_DIR/bin"
rm -rf "$RELEASE_DIR/bin.failed"
systemctl restart solana-hsm.service || true
systemctl start solana-hft.service
echo "rolled back; journalctl -u solana-hft -f"
