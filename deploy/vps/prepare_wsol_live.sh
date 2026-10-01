#!/usr/bin/env bash
set -euo pipefail

if [ ! -f /etc/solana-bot.env ]; then
  echo "Missing /etc/solana-bot.env" >&2
  exit 1
fi

set -a
. /etc/solana-bot.env
set +a

WALLET_PATH="${WALLET_PATH:-/opt/solana-bot/wallet.json}"
WSOL_MINT="So11111111111111111111111111111111111111112"

if ! command -v solana >/dev/null 2>&1; then
  echo "solana CLI not found" >&2
  exit 1
fi

if ! command -v spl-token >/dev/null 2>&1; then
  echo "spl-token CLI not found" >&2
  exit 1
fi

if [ ! -f "$WALLET_PATH" ]; then
  echo "Wallet file not found: $WALLET_PATH" >&2
  exit 1
fi

OWNER=$(solana-keygen pubkey "$WALLET_PATH")
ATA=$(spl-token --output json account-address --owner "$OWNER" --mint "$WSOL_MINT" 2>/dev/null || true)

printf "OWNER=%s\n" "$OWNER"
printf "WSOL_MINT=%s\n" "$WSOL_MINT"
printf "WSOL_ATA=%s\n" "${ATA:-<missing>}"
solana balance "$OWNER"

if [ -n "$ATA" ]; then
  echo "-- ATA account info --"
  spl-token --output json account-info "$ATA" || true
else
  echo "WSOL ATA is not created yet. Prepare it with:" >&2
  echo "  spl-token create-account $WSOL_MINT --owner $OWNER" >&2
  echo "  solana transfer --allow-unfunded-recipient $OWNER <lamports> --from $WALLET_PATH" >&2
  echo "  spl-token sync-native $ATA" >&2
fi
