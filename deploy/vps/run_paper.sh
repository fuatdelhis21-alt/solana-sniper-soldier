#!/usr/bin/env bash
set -euo pipefail

set -a
. /etc/solana-bot.env
set +a

exec /opt/solana-bot/bin/solana-sniper \
  --paper \
  --iterations 4294967295 \
  --rpc "$RPC_URL" \
  --ws "$WS_URL" \
  --ws-endpoint "$WS_URL" \
  --pool-id 3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv \
  --input-mint So11111111111111111111111111111111111111112 \
  --output-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --live-risk-data \
  --max-spend-sol 0.001 \
  --max-slippage-bps 50 \
  --data-dir /opt/solana-bot/data-paper