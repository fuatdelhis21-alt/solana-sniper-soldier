# Solana HFT Platform — Canlıya Alma Planı (Go-Live Plan)

> **Tarihsel belge.** Aşağıdaki "✅ Tamamlanan" etiketleri ilk geliştirme
> aşamalarını anlatır ve canlıya hazır olma kanıtı değildir (ör. Phase 3'teki
> `compute_output_amount()` yalnızca spot fiyat tahminidir; gerçek kotasyon artık
> `sim_quote` ile zincir simülasyonundan gelir). Güncel, kanıta dayalı durum:
> **[docs/READINESS.md](docs/READINESS.md)**.

## ✅ Tamamlanan (Phase 1-4)

### Phase 1: Execution & Signing Layer ✅
- `send_transfer` binary — SOL transfer CLI with `--dry-run`
- `BlockhashManager` + 3-level exponential backoff retry
- Transaction building with priority fee + compute budget
- `DecisionRecord` with JSONL + binary deterministic log

### Phase 2: Risk Management ✅
- `RiskManager` with max trade size, slippage bounds, daily loss circuit breaker (bincode persistence)
- Pre-trade check integrated into executor pipeline

### Phase 3: Market Data & AMM Analysis ✅
- **Real WebSocket** `programSubscribe` for Raydium CLMM (auto-reconnect + RPC fallback)
- **Real pool state parser**: sqrt_price, liquidity, tick_current_index, fee_rate
- `sqrt_price_to_price()`, `compute_output_amount()` with fee deduction
- Unit tests for pool parsing and price computation

### Phase 4: Observability & Monitoring ✅
- JSON structured logging with **daily file rotation** (`tracing-appender`)
- Metrics snapshot every 10 trades (total, success, avg latency) persisted to JSONL
- Critical error alerts: circuit breaker trip + RPC disconnect → stdout + JSON log

### Phase 5 Execution Layer Refinements ✅
- **Jito Bundle Sender**: Real HTTP POST to `/api/v1/bundle` with `reqwest` blocking client
- `.env.example` with mainnet-ready configuration (RPC, WS, Jito, risk limits)

---

## ❌ Eksik (Phase 5 — Validation & Go-Live)

### 5.1 Generate Wallet + Airdrop Devnet SOL
```bash
# Generate wallet.json (devnet)
solana-keygen new --outfile ./wallet.json --force
# View pubkey
solana-keygen pubkey ./wallet.json
# Airdrop devnet SOL
solana airdrop 2 <PUBKEY> --url https://api.devnet.solana.com
# Check balance
solana balance <PUBKEY> --url https://api.devnet.solana.com
```

### 5.2 Devnet Canary Test #1 — 0.001 SOL Transfer
```bash
# Dry-run first
cargo run --bin send_transfer -- ^
  --rpc https://api.devnet.solana.com ^
  --wallet ./wallet.json ^
  --to <DEST_PUBKEY> ^
  --amount 0.001 ^
  --dry-run

# Live send (0.001 SOL)
cargo run --bin send_transfer -- ^
  --rpc https://api.devnet.solana.com ^
  --wallet ./wallet.json ^
  --to <DEST_PUBKEY> ^
  --amount 0.001
```

### 5.3 Devnet Canary Test #2 — Simulation Mode
```bash
# Run simulation with 10 iterations (no wallet needed)
cargo run --bin solana-sniper -- --iterations 10
```

### 5.4 Devnet Canary Test #3 — Dry-Run Mode
```bash
cargo run --bin solana-sniper -- ^
  --rpc https://api.devnet.solana.com ^
  --ws wss://api.devnet.solana.com ^
  --wallet ./wallet.json ^
  --dry-run ^
  --iterations 5
```

### 5.5 Mainnet Go-Live Steps
Ayrıntılar: [`README_DEPLOY.md`](README_DEPLOY.md).
1. `./scripts/build_and_publish.sh` → paketi sunucuya `install_release.sh` ile kur
2. `generate_certs.sh` ile mTLS PKI, `hsm.env` içine `HSM_KEY_B64`, `systemctl start solana-hsm`
3. `/etc/solana-hft/solana-hft.env`: mainnet RPC/WS, `SNIPER_POOL_ID`, `SNIPER_OUTPUT_MINT`,
   `SNIPER_EXPECTED_SIGNER`, `SNIPER_LIVE_RISK_DATA=true`
4. `SNIPER_PAPER=true` → `SNIPER_DRY_RUN=true` → `SNIPER_LIVE=true` + `SNIPER_CONFIRM_LIVE=true`
5. İzleme: `journalctl -u solana-hft -f`, `curl 127.0.0.1:9090/metrics`
6. Kill switch / günlük limit tetiklenirse bot durur (exit 0); inceleyip `risk_state.json`'ı temizle

### 5.6 Final Mainnet Go-Live Checklist
- [ ] Cüzdan mainnet SOL ile fonlandı (≥ 0.02 SOL: işlem + ücret + ATA rent)
- [ ] Ücretli mainnet RPC (Helius/Triton/QuickNode) `SNIPER_RPC`/`SNIPER_WS`'te
- [ ] HSM `HSM_REQUIRE_KEY=1` ile çalışıyor, pubkey = `SNIPER_EXPECTED_SIGNER`
- [ ] Paper ve dry-run aynı havuzda hatasız çalıştı (dry-run tx'i imzalandı)
- [ ] `/metrics` ve `/health` izleniyor, alert kuralları yüklü
- [ ] Risk limitleri (kodda sabit): 0.05 SOL/işlem, 5 işlem/gün, 0.20 SOL günlük zarar, 1 açık pozisyon
- [ ] Jito (opsiyonel): `SNIPER_JITO_ENDPOINT`, `SNIPER_JITO_TIP_LAMPORTS`
