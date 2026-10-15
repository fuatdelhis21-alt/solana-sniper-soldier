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
BOT_UNIT=solana-hft.service
if systemctl is-active --quiet solana-hft-sniper.service; then
  BOT_UNIT=solana-hft-sniper.service
fi
systemctl stop "$BOT_UNIT"
mv "$RELEASE_DIR/bin" "$RELEASE_DIR/bin.failed"
mv "$RELEASE_DIR/bin.prev" "$RELEASE_DIR/bin"
rm -rf "$RELEASE_DIR/bin.failed"
systemctl restart solana-hsm.service || true
systemctl start "$BOT_UNIT"
# A bin.prev from an install older than the REVISION file has none.
echo "rolled back to revision: $(cat "$RELEASE_DIR/bin/REVISION" 2>/dev/null || echo unknown)"
echo "journalctl -u ${BOT_UNIT%.service} -f"
