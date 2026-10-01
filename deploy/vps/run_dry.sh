#!/usr/bin/env bash
set -euo pipefail
set -a
. /etc/solana-bot.env
set +a
cd /opt/solana-bot
exec ./target/release/solana-sniper \
  --dry-run \
  --iterations 1 \
  --rpc "$RPC_URL" \
  --ws "$WS_URL" \
  --hsm-endpoint https://127.0.0.1:8443 \
  --hsm-ca /opt/solana-bot/certs/ca.pem \
  --hsm-client-identity /opt/solana-bot/certs/client_all.pem \
  --pool-id 3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv \
  --input-mint So11111111111111111111111111111111111111112 \
  --output-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
  --live-risk-data \
  --max-spend-sol 0.001 \
  --max-slippage-bps 50 \
  --data-dir /opt/solana-bot/data
