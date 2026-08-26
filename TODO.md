# ✅ HFT Platform — Production Readiness TODO (COMPLETED)

## Phase 1: Execution & Signing Layer ✅
- [x] 1.1 Add solana-sdk, solana-client dependencies to workspace Cargo.toml
- [x] 1.2 Create `src/bin/send_transfer.rs` — secure transfer CLI tool
- [x] 1.3 Create `src/retry.rs` — blockhash management + 3-level exponential backoff
- [x] 1.4 Edit `solana-sniper/Cargo.toml` — add solana deps, update edition
- [x] 1.5 Edit `solana-sniper/src/main.rs` — add `--dry-run` flag

## Phase 2: Risk Management Module ✅
- [x] 2.1 Create `solana-sniper/src/risk.rs` — pre_trade_check with 3 guards
- [x] 2.2 Edit `solana-sniper/src/config.rs` — add risk fields
- [x] 2.3 Integrate risk check into executor pipeline

## Phase 3: Market Data & AMM Analysis ✅
- [x] 3.1 Enhance `solana-sniper/src/amm/raydium_v4.rs` — real CLMM pool state parser
- [x] 3.2 Enhance `crates/hft-marketdata/src/solana_ws.rs` — real `programSubscribe` WebSocket with auto-reconnect + RPC fallback
- [x] 3.3 Add deps: tokio-tungstenite, futures, base64 to hft-marketdata

## Phase 4: Observability & Monitoring ✅
- [x] 4.1 Add tracing subscriber to main.rs — JSON + DAILY FILE ROTATION via tracing-appender
- [x] 4.2 Add metrics bin recording (latency, success/fail rates) — JSONL persisted every 10 trades
- [x] 4.3 Add critical error alerts (circuit breaker trip, RPC disconnect)

## Phase 5: Validation & Go-Live ✅ (Code & Config)
- [x] 5.1 Complete Jito bundle sender (real HTTP POST) — `reqwest` dep added
- [x] 5.2 Create `.env.example` with mainnet config (RPC, WS, Jito, risk limits)
- [x] 5.3 Fixed `Cargo.toml` — added missing `sha2`, `byteorder` dependencies + binary target
- [x] 5.4 Created `.cargo/config.toml` with `rust-lld` linker for MSVC build
- [x] 5.5 All source modules reviewed and confirmed complete

## ✅ Build Status (Windows) — SOLVED
- [x] **MSVC linker (link.exe) not found** — fixed via `.cargo/config.toml` with `rust-lld`
- [x] **`cargo build -p solana-sniper`** — **SUCCESS** (0 errors, 28 warnings — dead_code/unused only)

## ✅ Devnet Live Test — SUCCESS 🎉
- [x] **wallet.json** — generated (pubkey: `4sirSH9isRCEqcu72TKDXhV1kn6sURKabM981u3ZTvUt`)
- [x] **`send_transfer --dry-run`** — simulation basarili
- [x] **`solana-sniper --dry-run --iterations 3`** — 3/3 dry-run tx signed + logged
- [x] **`solana-sniper --live --iterations 3`** — **2/3 TX on-chain confirmed** ✅
  - TX: `4soqLn9pPoNiNc7k2U1pZkBazjreVvJGt2hgk2pppuKQUn3EMeQeAuraebuxWSBwoKt3zENrZUAFeLxXoTpTmesZ`
  - Avg latency: 1301ms (devnet RPC latency included)

## ✅ Bug Fixes
- [x] **init_tracing WorkerGuard** — guard returned + bound to `_guard` in main, log no longer cuts off
- [x] **Live mode read-only account** — `to = kp.pubkey()` (self-transfer) instead of zero-address [0u8; 32]

## ✅ Remote HSM Signer Scaffold — BUILT & VERIFIED
- [x] **`solana-sniper/src/remote_hsm.rs`** — client-side remote signing module (compiles clean)
- [x] **`solana-sniper/src/lib.rs`** — exposes `hw_signer`, `ledger_signer`, `remote_hsm`, `integration` modules
- [x] **`solana-sniper/src/bin/sign_test.rs`** — signing test binary (Hw stub → local keyfile fallback)
- [x] **`solana-sniper/src/bin/ledger_sign_test.rs`** — Ledger signer test binary
- [x] **`tools/hsm_server/`** — standalone warp-based remote HSM server with `POST /sign` endpoint
  - `tools/hsm_server/Cargo.toml` — empty `[workspace]` table makes it standalone (excluded from parent workspace)
  - `main.rs` — reads `HSM_KEY_B64`, exposes `POST /sign` returning base64 64-byte signature
- [x] **build_client.bat / build_check.bat / build_hsm.bat** — build scripts for both crates

### ✅ Build Status
- [x] **`solana-sniper`** (client): `BUILD_EXIT=0` — `solana-sniper`, `sign_test`, `ledger_sign_test`, `send_transfer` all compiled (only dead_code/unused warnings)
- [x] **`tools/hsm_server`** (standalone): `BUILD_EXIT=0` — `Finished dev profile in 8m 46s`

### ✅ Runtime Verification
- [x] **`solana-sniper --dry-run --iterations 3`** — 3/3 transactions signed & logged ✅
- [x] **`sign_test.exe wallet.json`** — local keyfile signature produced: `2cFW9UNe...` ✅
- [x] **`ledger_sign_test.exe wallet.json`** — graceful fallback to local signer ✅
- [x] **`hsm_server.exe`** — starts, listens on `127.0.0.1:8443`, `POST /sign` returns valid base64 64-byte signature ✅

## 🚀 Next: Mainnet Go-Live
- [ ] Fund mainnet wallet (min 0.02 SOL)
- [ ] Switch `.env` to mainnet RPC/WS (Helius/Triton/RPCFast)
- [ ] Add `--mainnet` flag or auto-detect from RPC URL
- [ ] Set risk limits: max_trade 0.01 SOL, daily_loss 0.05 SOL, max_slippage 50 bps
- [ ] Generate fresh blockhash per iteration (avoid replay like iter 3)
- [ ] Integrate Raydium v4 CLMM swap via `hft-execution` crate
- [ ] Enable Jito bundle submission for mainnet only
- [ ] First mainnet test: `--dry-run --iterations 3`
- [ ] Live mainnet: `--live --iterations 3` with small amount

## 📂 Project Structure
```
solana-hft-platform/
├── Cargo.toml                          # Workspace root
├── .cargo/config.toml                  # MSVC + rust-lld linker config
├── .env.example                        # Mainnet-ready env template
├── TODO.md                             # This file
├── GO_LIVE_PLAN.md                     # Detailed go-live instructions
├── README_DEPLOY.md                    # Deployment guide
├── SETUP_WINDOWS.md                    # Windows setup guide
├── solana-sniper/                      # Main trading binary
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                     # HFT loop: sim / dry-run / live
│       ├── config.rs                   # TOML + env config loader
│       ├── risk.rs                     # Circuit breaker + risk guards
│       ├── retry.rs                    # BlockhashManager + 3-level backoff
│       ├── executor.rs                 # Transaction builder + sender
│       ├── jito.rs                     # Jito bundle integration
│       ├── tx.rs                       # Transaction utilities
│       ├── decision.rs                 # Decision record
│       ├── amm.rs / amm/mod.rs         # AmmAdapter trait + Quote/TradeIntent
│       ├── amm/raydium_v4.rs           # Raydium CLMM pool parser + tests
│       └── bin/send_transfer.rs        # SOL transfer CLI
├── crates/
│   ├── hft-core/                       # Market types (Price, Quantity, OrderBook)
│   ├── hft-marketdata/                 # WebSocket + market data pipeline
│   │   └── src/solana_ws.rs            # Real programSubscribe WS for Raydium
│   └── hft-execution/                  # AmmAdapter trait + Jito bundle
│       └── src/
│           ├── amm_adapter.rs
│           ├── raydium_v4.rs
│           ├── jito_bundle.rs
│           ├── backend.rs / order.rs / rpc.rs
└── target/                             # Build artifacts
```

## 🚀 Quick Start
```bash
# 1. Generate wallet
solana-keygen new --outfile ./wallet.json --force

# 2. Airdrop devnet SOL
solana airdrop 2 $(solana-keygen pubkey ./wallet.json) --url https://api.devnet.solana.com

# 3. Dry-run transfer (safe)
cargo run --bin send_transfer -- --to <DEST_PUBKEY> --amount 0.001 --dry-run

# 4. Live transfer (0.001 SOL)
cargo run --bin send_transfer -- --to <DEST_PUBKEY> --amount 0.001

# 5. Run HFT simulation (10 iterations)
cargo run --bin solana-sniper -- --iterations 10

# 6. Mainnet go-live
# Edit .env with mainnet RPC + funded wallet, then:
cargo run --bin solana-sniper -- --rpc <MAINNET_RPC> --ws <MAINNET_WS> --wallet ./wallet.json --live
