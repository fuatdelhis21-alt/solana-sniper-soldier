#!/usr/bin/env bash
set -euo pipefail

wallet_file="${1:-/dev/shm/solana-wallet-setup.json}"
expected_pubkey="9h9FryJbsj8BHvrF26hPxtDur596Ai8WjYVGyoq4DADR"
solana_keygen="${SOLANA_KEYGEN:-/root/.local/share/solana/install/releases/stable-e29e5d910f0c2b7176f58174e592e8488099ef75/solana-release/bin/solana-keygen}"
actual_pubkey="$("$solana_keygen" pubkey "$wallet_file")"
[[ "$actual_pubkey" == "$expected_pubkey" ]] || {
  printf 'Wallet pubkey mismatch; refusing to change HSM key.\n' >&2
  exit 1
}
cp -a /etc/solana-hsm.env "/root/solana-hsm.env.backup-$(date +%Y%m%d%H%M%S)"

python3 - "$wallet_file" "$expected_pubkey" <<'PY'
import base64
import json
import os
import pathlib
import sys
import tempfile

wallet_path = pathlib.Path(sys.argv[1])
expected_pubkey = sys.argv[2]
secret = bytes(json.loads(wallet_path.read_text(encoding="utf-8")))
if len(secret) != 64:
    raise SystemExit("wallet keypair must decode to exactly 64 bytes")

env_path = pathlib.Path("/etc/solana-hsm.env")
lines = env_path.read_text(encoding="utf-8").splitlines()
lines = [line for line in lines if not line.startswith("HSM_KEY_B64=")]
lines.append("HSM_KEY_B64=" + base64.b64encode(secret).decode("ascii"))
fd, temp_name = tempfile.mkstemp(prefix="solana-hsm.env.", dir=str(env_path.parent))
try:
    with os.fdopen(fd, "w", encoding="utf-8") as output:
        output.write("\n".join(lines) + "\n")
        output.flush()
        os.fsync(output.fileno())
    os.chmod(temp_name, 0o600)
    os.chown(temp_name, 0, 0)
    os.replace(temp_name, env_path)
finally:
    if os.path.exists(temp_name):
        os.unlink(temp_name)

print("HSM key source updated for expected wallet " + expected_pubkey)
PY

systemctl restart solana-hsm.service
systemctl is-active --quiet solana-hsm.service
set -a
. /etc/solana-bot.env
set +a
hsm_pubkey="$(curl --silent --show-error --fail --max-time 10 \
  --cacert "$HSM_CA" --cert "$HSM_CLIENT_IDENTITY" --key "$HSM_CLIENT_IDENTITY" \
  "${HSM_ENDPOINT%/}/pubkey" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["pubkey"])')"
[[ "$hsm_pubkey" == "$expected_pubkey" ]] || {
  printf 'HSM pubkey mismatch after restart: %s\n' "$hsm_pubkey" >&2
  exit 1
}
printf 'HSM signer verified: %s\n' "$hsm_pubkey"
