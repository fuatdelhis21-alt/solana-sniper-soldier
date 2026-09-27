=== VPS KURULUM ADIMLAR ===

# Solana HFT Bot — VPS'te Çalıştırma

## 1. Binary'yi VPS'e Kopyalayın

### Windows'tan Linux VPS'e:
```bash
# Windows'ta (PowerShell):
scp target/x86_64-pc-windows-msvc/release/solana-sniper.exe user@vps_ip:/home/user/solana-hft/

# VEYA Linux'ta derleyin:
cargo build --release
# Binary: target/release/solana-sniper
```

## 2. VPS'te .env Dosyasını Oluşturun

```bash
ssh user@vps_ip
cd /home/user/solana-hft
cat > .env << 'ENVFILE'
# === RPC & WebSocket ===
RPC_URL=<VPS'teki RPC endpoint>
WS_URL=<VPS'teki WebSocket endpoint>

# === Wallet ===
WALLET_PATH=/home/user/solana-hft/wallet.json

# === Risk Limits ===
MAX_TRADE_SIZE_SOL=0.01
DAILY_LOSS_LIMIT_SOL=0.05
MAX_SLIPPAGE_BPS=50
DATA_DIR=./data

# === Jito (opsiyonel) ===
JITO_TPU_URL=frankfurt.mainnet.block-engine.jito.wtf
JITO_TIP_SOL=0.0001

# === Compute Budget ===
PRIORITY_FEE_MICRO_LAMPORTS=10000
COMPUTE_UNITS=500

# === Logging ===
LOG_LEVEL=info,solana_sniper=debug
ENVFILE
```

## 3. Wallet Dosyasını Kontrol Edin

```bash
# VPS'te wallet.json var mı?
ls -la /home/user/solana-hft/wallet.json

# Wallet'ta SOL var mı?
solana balance -k /home/user/solana-hft/wallet.json -u m
```

## 4. Bot'u Çalıştırın

### Dry-run (işlem gönderme yok, test):
```bash
./solana-sniper \
  --pool-id <MAINNET_POOL_ID> \
  --input-mint EPjFWdd5Au... \
  --output-mint <TOKEN_MINT> \
  --dry-run \
  --iterations 5
```

### Live (gerçek işlem):
```bash
./solana-sniper \
  --pool-id <MAINNET_POOL_ID> \
  --input-mint EPjFWdd5Au... \
  --output-mint <TOKEN_MINT> \
  --live \
  --iterations 100
```

### Paper (simülasyon, gerçek fiyatlar):
```bash
./solana-sniper \
  --pool-id <MAINNET_POOL_ID> \
  --input-mint EPjFWdd5Au... \
  --output-mint <TOKEN_MINT> \
  --paper \
  --iterations 50
```

## 5. Logs'u İzleyin

```bash
# Gerçek zamanlı logs:
tail -f data/solana-sniper.log

# Veya:
tail -f data/audit/risk_audit.jsonl
```

## 6. Arka Planda Çalıştırın (tmux/screen)

```bash
# tmux ile:
tmux new-session -d -s hft-bot './solana-sniper --pool-id ... --live'
tmux attach -t hft-bot

# screen ile:
screen -S hft-bot
./solana-sniper --pool-id ... --live
# Ctrl+A, D ile detach
```

## 7. Systemd Service (Opsiyonel)

```bash
sudo cat > /etc/systemd/system/solana-hft.service << 'SVCFILE'
[Unit]
Description=Solana HFT Bot
After=network.target

[Service]
Type=simple
User=user
WorkingDirectory=/home/user/solana-hft
ExecStart=/home/user/solana-hft/solana-sniper --pool-id <POOL_ID> --input-mint ... --output-mint ... --live
Restart=on-failure
RestartSec=10

[Install]
WantedBy=multi-user.target
SVCFILE

sudo systemctl daemon-reload
sudo systemctl enable solana-hft
sudo systemctl start solana-hft
sudo systemctl status solana-hft
```

## Önemli Notlar

- **Wallet private key'i asla commit'lemeyin**
- **Mainnet'te küçük miktarlarla başlayın** (0.01 SOL)
- **Circuit breaker** 0.05 SOL zarar sonrası otomatik durdurur
- **Dry-run** ile işlemleri test edin
- **Logs'u** düzenli kontrol edin

