#!/usr/bin/env bash
# Install a release artifact produced by scripts/build_and_publish.sh.
#
#   sudo ./install_release.sh <solana-hft-release-*.tar.gz> [solana-hft.env]
#
# Layout after install:
#   /opt/solana-hft/bin/{solana-sniper,pool-sniper,hsm_server,send_transfer}
#   /opt/solana-hft/bin.prev/            previous binaries (deploy/rollback.sh)
#   /etc/systemd/system/solana-hft.service, solana-hsm.service
#   /etc/solana-hft/solana-hft.env       bot config (SNIPER_*), 0600 solana
#   /etc/solana-hft/hsm.env              HSM_KEY_B64, 0600 solana-hsm
#   /etc/solana-hft/hsm-certs/           server.pem server.key ca.pem (solana-hsm)
#   /etc/solana-hft/client-certs/        ca.pem client_all.pem (solana)
#   /var/lib/solana-hft/                 bot state: risk_state.json, live_position.json, logs
#
# Services are (re)started only once they are configured; a fresh install
# prints the remaining manual steps instead of starting an unconfigured bot.
set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "Usage: $0 <artifact.tar.gz> [solana-hft.env]" >&2
  echo "(a glob like solana-hft-release-*.tar.gz must match exactly one file)" >&2
  exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
  echo "must run as root" >&2
  exit 2
fi

ARTIFACT="$1"
ENV_SRC="${2:-}"
# The env file must be a small text file — never another artifact (e.g. when
# a glob matched two release tarballs and the second one landed here).
if [ -n "$ENV_SRC" ]; then
  if [ ! -f "$ENV_SRC" ] || ! grep -Iq . "$ENV_SRC" || [ "$(wc -c < "$ENV_SRC")" -gt 65536 ]; then
    echo "refusing env file '$ENV_SRC': not a small text file (did a glob match several tarballs?)" >&2
    exit 2
  fi
fi
RELEASE_DIR=/opt/solana-hft
BIN_DIR="$RELEASE_DIR/bin"
CONFIG_DIR=/etc/solana-hft
BOT_ENV="$CONFIG_DIR/solana-hft.env"
HSM_ENV="$CONFIG_DIR/hsm.env"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
tar -xzf "$ARTIFACT" -C "$TMPDIR"
if [ -f "$TMPDIR/SHA256SUMS" ]; then
  (cd "$TMPDIR" && sha256sum -c --quiet SHA256SUMS)
  echo "artifact checksums OK"
fi

for u in solana solana-hsm; do
  id -u "$u" >/dev/null 2>&1 || useradd -r -s /usr/sbin/nologin -d /nonexistent "$u"
done

mkdir -p "$BIN_DIR"
if [ -n "$(ls -A "$BIN_DIR" 2>/dev/null)" ]; then
  rm -rf "$RELEASE_DIR/bin.prev"
  cp -a "$BIN_DIR" "$RELEASE_DIR/bin.prev"
fi
install -m 0755 "$TMPDIR"/bin/* "$BIN_DIR"/
# The commit the binaries were built from lives next to them, so bin.prev and
# rollback.sh carry it along; /opt/solana-hft/REVISION always points at the
# active one.
if [ -f "$TMPDIR/REVISION" ]; then
  install -m 0644 "$TMPDIR/REVISION" "$BIN_DIR/REVISION"
else
  echo "unknown (artifact has no REVISION)" > "$BIN_DIR/REVISION"
fi
ln -sfn bin/REVISION "$RELEASE_DIR/REVISION"
echo "installing revision: $(cat "$BIN_DIR/REVISION")"
for f in README_DEPLOY.md solana-hft.env.example hsm.env.example generate_certs.sh rollback.sh \
  sniper_report.py screen_candidates.py; do
  [ -f "$TMPDIR/$f" ] && install -m 0644 "$TMPDIR/$f" "$RELEASE_DIR/$f"
done
chmod 0755 "$RELEASE_DIR"/*.sh "$RELEASE_DIR"/*.py 2>/dev/null || true

# Only our own units (never a same-named service of another bot on the host).
for u in solana-hft.service solana-hft-sniper.service solana-hsm.service; do
  install -m 0644 "$TMPDIR/systemd/$u" "/etc/systemd/system/$u"
done

install -d -m 0755 "$CONFIG_DIR"
install -d -m 0700 -o solana-hsm -g solana-hsm "$CONFIG_DIR/hsm-certs"
install -d -m 0700 -o solana -g solana "$CONFIG_DIR/client-certs"

BOT_CONFIGURED=0
if [ -n "$ENV_SRC" ]; then
  install -m 0600 -o solana -g solana "$ENV_SRC" "$BOT_ENV"
  BOT_CONFIGURED=1
elif [ -f "$BOT_ENV" ]; then
  BOT_CONFIGURED=1
else
  install -m 0600 -o solana -g solana "$TMPDIR/solana-hft.env.example" "$BOT_ENV"
fi
[ -f "$HSM_ENV" ] || install -m 0600 -o solana-hsm -g solana-hsm "$TMPDIR/hsm.env.example" "$HSM_ENV"

systemctl daemon-reload
# The new-pool sniper (solana-hft-sniper) and the fixed-pool bot
# (solana-hft) share wallet and state and conflict: keep whichever one the
# operator chose. A fresh install enables the fixed-pool bot.
BOT_UNIT=solana-hft.service
if systemctl is-enabled --quiet solana-hft-sniper.service 2>/dev/null \
  || systemctl is-active --quiet solana-hft-sniper.service 2>/dev/null; then
  BOT_UNIT=solana-hft-sniper.service
fi
systemctl enable solana-hsm.service "$BOT_UNIT" >/dev/null

HSM_READY=0
if grep -Eq '^HSM_KEY_B64=.+' "$HSM_ENV" && [ -f "$CONFIG_DIR/hsm-certs/server.key" ]; then
  HSM_READY=1
  systemctl restart solana-hsm.service
fi

if [ "$BOT_CONFIGURED" = 1 ]; then
  systemctl restart "$BOT_UNIT"
  echo "Install complete — ${BOT_UNIT%.service} restarted. Logs: journalctl -u ${BOT_UNIT%.service} -f"
else
  cat <<MSG
Install complete — services enabled but NOT started (not configured yet):
  1. Certs:  $RELEASE_DIR/generate_certs.sh /root/hft-pki
             cp /root/hft-pki/{server.pem,server.key,ca.pem} $CONFIG_DIR/hsm-certs/
             cp /root/hft-pki/{ca.pem,client_all.pem} $CONFIG_DIR/client-certs/
             chown -R solana-hsm: $CONFIG_DIR/hsm-certs; chown -R solana: $CONFIG_DIR/client-certs
  2. Key:    set HSM_KEY_B64 in $HSM_ENV
  3. Config: edit $BOT_ENV (start with SNIPER_PAPER=true or SNIPER_DRY_RUN=true)
  4. Start:  systemctl start solana-hsm solana-hft
MSG
  [ "$HSM_READY" = 1 ] || echo "  (HSM not started: key or certs missing)"
fi
