#!/usr/bin/env bash
set -euo pipefail

set -a
. /etc/solana-bot.env
set +a

fail() { printf 'LIVE preflight failed: %s\n' "$1" >&2; exit 1; }

if [[ "${PREFLIGHT_ONLY:-false}" != true ]]; then
  [[ "${LIVE_TRADING_ENABLED:-false}" == true ]] || fail 'LIVE_TRADING_ENABLED must be true'
  [[ "${LIVE_TRADING_CONFIRMED:-false}" == true ]] || fail 'LIVE_TRADING_CONFIRMED must be true'
  [[ -f "${ARM_LIVE:-/opt/solana-bot/ARM_LIVE}" ]] || fail 'ARM_LIVE file is missing'
fi
[[ -n "${RPC_URL:-}" && -n "${WS_URL:-}" ]] || fail 'RPC_URL and WS_URL are required'
[[ -n "${MAX_POSITION_SOL:-}" && -n "${MAX_DAILY_LOSS_SOL:-}" && -n "${MAX_DAILY_TRADES:-}" ]] || fail 'all three risk limits are required'
[[ -n "${MAX_SLIPPAGE_BPS:-}" && -n "${DATA_DIR:-}" ]] || fail 'MAX_SLIPPAGE_BPS and DATA_DIR are required'
[[ -n "${POOL_ID:-}" && -n "${INPUT_MINT:-}" && -n "${OUTPUT_MINT:-}" ]] || fail 'POOL_ID and both mints are required'
[[ -n "${EXPECTED_WALLET_PUBKEY:-}" ]] || fail 'EXPECTED_WALLET_PUBKEY is required for HSM identity verification'
[[ -n "${HSM_ENDPOINT:-}" && -r "${HSM_CA:-}" && -r "${HSM_CLIENT_IDENTITY:-}" ]] || fail 'HSM mTLS endpoint or certificates are unavailable'
command -v curl >/dev/null || fail 'curl is required'
command -v python3 >/dev/null || fail 'python3 is required'
python3 - "${MAX_POSITION_SOL}" "${MAX_DAILY_LOSS_SOL}" "${MAX_DAILY_TRADES}" "${MAX_SLIPPAGE_BPS}" <<'PY' || fail 'risk limits are invalid or exceed production hard caps'
import sys

position, daily_loss = map(float, sys.argv[1:3])
daily_trades, slippage = map(int, sys.argv[3:5])
if not (0.01 <= position <= 0.05 and 0 < daily_loss <= 0.20 and 1 <= daily_trades <= 5 and 1 <= slippage <= 10_000):
    raise SystemExit(1)
PY

rpc_response="$(curl --silent --fail --max-time 15 \
  -H 'Content-Type: application/json' \
  --data-binary '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$RPC_URL" 2>/dev/null)" \
  || fail 'RPC health request failed'
rpc_health="$(printf '%s' "$rpc_response" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("result", ""))' 2>/dev/null)" \
  || fail 'RPC health response is invalid'
unset rpc_response
[[ "$rpc_health" == ok ]] || fail 'RPC getHealth did not return ok'

hsm_pubkey="$(curl --silent --fail --max-time 10 \
  --cacert "$HSM_CA" --cert "$HSM_CLIENT_IDENTITY" --key "$HSM_CLIENT_IDENTITY" \
  "${HSM_ENDPOINT%/}/pubkey" 2>/dev/null \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["pubkey"])' 2>/dev/null)" \
  || fail 'mTLS HSM pubkey request failed'
[[ "$hsm_pubkey" == "$EXPECTED_WALLET_PUBKEY" ]] || fail "HSM pubkey mismatch: expected $EXPECTED_WALLET_PUBKEY, got $hsm_pubkey"
if [[ "${PREFLIGHT_ONLY:-false}" == true ]]; then
  printf 'LIVE_PREFLIGHT_OK: RPC healthy; HSM signer matches expected wallet %s\n' "$EXPECTED_WALLET_PUBKEY"
  exit 0
fi

exec /opt/solana-bot/bin/solana-sniper \
  --live \
  --confirm-live \
  --iterations 4294967295 \
  --rpc "$RPC_URL" \
  --ws "$WS_URL" \
  --ws-endpoint "$WS_URL" \
  --hsm-endpoint "$HSM_ENDPOINT" \
  --hsm-ca "$HSM_CA" \
  --hsm-client-identity "$HSM_CLIENT_IDENTITY" \
  --pool-id "$POOL_ID" \
  --input-mint "$INPUT_MINT" \
  --output-mint "$OUTPUT_MINT" \
  --live-risk-data \
  --max-spend-sol "$MAX_POSITION_SOL" \
  --max-slippage-bps "$MAX_SLIPPAGE_BPS" \
  --data-dir "$DATA_DIR"
