#!/usr/bin/env bash
set -euo pipefail

if ! id -u bot_service >/dev/null 2>&1; then
  useradd --system --home-dir /home/bot_service --shell /usr/sbin/nologin bot_service
fi

install -d -o bot_service -g bot_service -m 750 /opt/solana-bot
install -d -o bot_service -g bot_service -m 750 /opt/solana-bot/data
install -d -o bot_service -g bot_service -m 750 /opt/solana-bot/logs
install -d -o bot_service -g bot_service -m 750 /opt/solana-bot/certs
install -d -o bot_service -g bot_service -m 700 /home/bot_service/.solana

if [ -f /etc/solana-bot.env ]; then
  chown root:bot_service /etc/solana-bot.env
  chmod 640 /etc/solana-bot.env
fi

if [ -f /opt/solana-bot/ARM_LIVE ]; then
  chown bot_service:bot_service /opt/solana-bot/ARM_LIVE
  chmod 600 /opt/solana-bot/ARM_LIVE
fi

chown -R bot_service:bot_service /opt/solana-bot /home/bot_service/.solana

echo "bot_service user ready"
