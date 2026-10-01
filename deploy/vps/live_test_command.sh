#!/usr/bin/env bash
set -euo pipefail

set -a
. /etc/solana-bot.env
set +a

exec /opt/solana-bot/target/release/solana-sniper \
  --live \
  --confirm-live \
  --iterations 1 \
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
  --max-spend-sol "$MAX_SPEND_SOL" \
  --max-slippage-bps "$MAX_SLIPPAGE_BPS" \
  --data-dir "$DATA_DIR"
