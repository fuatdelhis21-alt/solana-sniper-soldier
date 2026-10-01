# Deployment Checklist — Solana HFT Bot

## Pre-Deployment ✅

- [x] All 101 tests passing
- [x] Code reviewed and optimized
- [x] Risk limits configured (mainnet-safe)
- [x] HSM integration verified
- [x] RPC/WS endpoints configured
- [x] Wallet funded (0.1+ SOL for gas)
- [x] Audit logging enabled
- [x] Documentation complete

## Deployment Steps

### Step 1: Pull Latest Code
```bash
ssh bot_service@31.97.125.104
cd /opt/solana-sniper-soldier
git fetch origin
git checkout feat/perf-rpc-quote-cache
git pull origin feat/perf-rpc-quote-cache
```
- [ ] Code pulled successfully
- [ ] No merge conflicts
- [ ] Branch is `feat/perf-rpc-quote-cache`

### Step 2: Build Binary
```bash
cargo build --release 2>&1 | tail -50
```
- [ ] Build completed without errors
- [ ] Binary size: ~16 MB (Linux)
- [ ] All dependencies resolved

### Step 3: Restart Service
```bash
sudo systemctl restart solana-sniper-soldier.service
sudo systemctl status solana-sniper-soldier.service
```
- [ ] Service restarted successfully
- [ ] Status shows "active (running)"
- [ ] No error messages

### Step 4: Verify Deployment
```bash
# Check logs
tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*

# Check audit trail
tail -50 /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl

# Verify running
ps aux | grep solana-sniper
```
- [ ] Logs show bot starting
- [ ] Audit trail recording events
- [ ] Process running with correct PID

## Post-Deployment Monitoring

### Hour 1: Baseline
- [ ] Bot running without errors
- [ ] RPC connectivity verified
- [ ] HSM signing working
- [ ] Pump detection active

### Hour 2: First Trade
- [ ] Pump detected (pump_score > 50)
- [ ] Entry signal generated
- [ ] Position opened
- [ ] Audit log recording trade

### Hour 3+: Ongoing
- [ ] Position monitoring active
- [ ] Exit signals evaluated
- [ ] P&L tracking
- [ ] Risk limits enforced

## Monitoring Commands

### Real-time Metrics
```bash
tail -f /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | jq '.'
```

### Health Checks
```bash
# RPC health
curl -s https://mainnet.helius-rpc.com -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | jq '.result'

# HSM health
curl -s --cacert /etc/solana-bot/hsm/ca.crt \
  --cert /etc/solana-bot/hsm/client.crt \
  --key /etc/solana-bot/hsm/client.key \
  https://127.0.0.1:8443/pubkey | jq '.pubkey'

# Service status
sudo systemctl status solana-sniper-soldier.service
```

## Rollback Plan

If issues occur:

### Option 1: Revert to Previous Version
```bash
cd /opt/solana-sniper-soldier
git checkout 23783a4  # Previous stable commit
cargo build --release
sudo systemctl restart solana-sniper-soldier.service
```

### Option 2: Emergency Stop
```bash
sudo systemctl stop solana-sniper-soldier.service
# Positions will be closed on next restart
```

### Option 3: Manual Position Close
```bash
# Use dry-run mode to close positions
./target/release/solana-sniper --dry-run --close-positions
```

## Success Criteria

- [x] Bot starts without errors
- [x] RPC/HSM connectivity verified
- [x] Pump detection active
- [ ] First trade within 2 hours
- [ ] Position management working
- [ ] Exit signals triggered
- [ ] P&L tracking accurate
- [ ] Risk limits enforced

## Troubleshooting

### No Trades After 2 Hours
1. Check pump detection: `grep "pump_score" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20`
2. Check liquidity: `grep "liquidity" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20`
3. Check holder gates: `grep "holder_concentration" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20`

### High Slippage
1. Check pool liquidity
2. Reduce position size
3. Increase slippage tolerance

### Position Not Closing
1. Check exit signals: `grep "should_exit" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20`
2. Check TP/SL thresholds
3. Verify price feed

## Sign-Off

- [ ] Deployment completed
- [ ] All checks passed
- [ ] Monitoring active
- [ ] Ready for production

**Deployed by:** _______________
**Date:** _______________
**Time:** _______________

---

**For support, see HFT_DEPLOYMENT_GUIDE.md**
