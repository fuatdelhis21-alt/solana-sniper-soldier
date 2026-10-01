#!/usr/bin/env bash
set -euo pipefail

install -d -o hsm_service -g hsm_service -m 750 /opt/solana-hsm/logs
chown root:root /opt/solana-hsm/hsm_signing_key.json
chmod 600 /opt/solana-hsm/hsm_signing_key.json

python3 - <<'PY'
import base64
import json

with open('/opt/solana-hsm/hsm_signing_key.json', encoding='utf-8') as handle:
    key = bytes(json.load(handle))
if len(key) != 64:
    raise SystemExit('HSM key must contain exactly 64 bytes')
with open('/etc/solana-hsm.env', 'w', encoding='utf-8') as handle:
    handle.write('HSM_KEY_B64=' + base64.b64encode(key).decode('ascii') + '\n')
PY

chown root:root /etc/solana-hsm.env
chmod 600 /etc/solana-hsm.env

cat > /etc/systemd/system/solana-hsm.service <<'UNIT'
[Unit]
Description=Solana HSM signer
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=hsm_service
Group=hsm_service
WorkingDirectory=/opt/solana-hsm
EnvironmentFile=/etc/solana-hsm.env
ExecStart=/opt/solana-hsm/target/release/hsm_server --certs /opt/solana-hsm/certs --log-file /opt/solana-hsm/logs/hsm_audit.log
Restart=on-failure
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/solana-hsm/logs

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now solana-hsm.service
systemctl is-active --quiet solana-hsm.service
echo 'solana-hsm.service active'