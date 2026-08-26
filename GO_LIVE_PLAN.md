# Solana HFT Platform — Canlıya Alma Planı (Go-Live Plan)

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
1. Copy `.env.example` → `.env` and fill mainnet values
2. Fund mainnet wallet with ~0.1 SOL (conservative start)
3. Start with `--dry-run` first to verify tx format
4. Start live with `--live` flag + small amount
5. Monitor `data/logs/hft.log` and `data/metrics.jsonl`
6. If circuit breaker trips, investigate and reset `data/circuit_breaker.bin`

### 5.6 Final Mainnet Go-Live Checklist
- [ ] wallet.json funded with mainnet SOL (minimum 0.02 SOL for fees)
- [ ] `.env` configured with mainnet RPC (no free tier — use Helius/Triton/RPCFast)
- [ ] Risk limits set: `MAX_TRADE_SIZE_SOL=0.01`, `DAILY_LOSS_LIMIT_SOL=0.05`
- [ ] Circuit breaker tested: run `--dry-run` first
- [ ] File rotation logging active: `data/logs/hft.log.*`
- [ ] Jito TPU URL set: `JITO_TPU_URL=frankfurt.mainnet.block-engine.jito.wtf`
- [ ] Raydium CLMM pool IDs configured in config (or auto-discover via WS)
- [ ] Bot started as Windows scheduled task or manual terminal
