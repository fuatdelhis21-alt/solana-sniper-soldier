# Live Ops: WSOL + Helius + service hardening

## 1) Prepare bot_service user

```bash
sudo bash /opt/solana-bot/setup_bot_user.sh
```

## 2) Create /etc/solana-bot.env

```bash
sudo cp /opt/solana-bot/live_env.example /etc/solana-bot.env
sudo nano /etc/solana-bot.env
```

Required values:
- RPC_URL
- WS_URL
- HSM_ENDPOINT
- HSM_CA
- HSM_CLIENT_IDENTITY
- POOL_ID
- INPUT_MINT = So11111111111111111111111111111111111111112
- OUTPUT_MINT = target pool output mint
- MAX_SPEND_SOL = 0.01
- MAX_SLIPPAGE_BPS = 50

## 3) Prepare WSOL ATA

```bash
sudo bash /opt/solana-bot/prepare_wsol_live.sh
```

If ATA is missing, create it and sync native:

```bash
solana-keygen pubkey /opt/solana-bot/wallet.json
spl-token create-account So11111111111111111111111111111111111111112 --owner <OWNER>
solana transfer --allow-unfunded-recipient <OWNER> 10000000 --from /opt/solana-bot/wallet.json
spl-token sync-native <WSOL_ATA>
```

## 4) Service arm

```bash
sudo touch /opt/solana-bot/ARM_LIVE
sudo chown bot_service:bot_service /opt/solana-bot/ARM_LIVE
sudo chmod 600 /opt/solana-bot/ARM_LIVE
```

## 5) Live test command

```bash
sudo bash /opt/solana-bot/live_test_command.sh
```

This is only for a single live iteration, with the HSM and pool validation path active.

## 6) Service start

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now solana-bot.service
sudo systemctl status solana-bot.service
journalctl -u solana-bot.service -n 100 --no-pager
```

## 7) Security note

The bot must not run as root in production. The service is configured to use `bot_service` and to require `/opt/solana-bot/ARM_LIVE` before doing a live run.
