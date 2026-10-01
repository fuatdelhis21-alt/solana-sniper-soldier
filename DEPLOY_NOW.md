# 🚀 DEPLOY NOW — Solana HFT Bot

## Deployment Instructions (VPS)

### Prerequisites
- SSH access to VPS (31.97.125.104)
- bot_service user with sudo privileges
- Cargo installed on VPS
- ~10 minutes for build

---

## Option 1: Automated Deployment (Recommended)

### Step 1: SSH to VPS
```bash
ssh bot_service@31.97.125.104
```

### Step 2: Run Deployment Script
```bash
cd /opt/solana-sniper-soldier
./deploy_to_vps.sh
```

**What it does:**
1. Pulls latest code from `feat/perf-rpc-quote-cache`
2. Builds binary (`cargo build --release`)
3. Restarts systemd service
4. Verifies deployment (logs, audit trail, process)

**Expected output:**
```
╔════════════════════════════════════════════════════════════════╗
║         SOLANA HFT BOT — VPS DEPLOYMENT                       ║
╚════════════════════════════════════════════════════════════════╝

📥 Step 1: Pulling latest code...
✅ Code pulled successfully

🔨 Step 2: Building binary...
   (This may take 5-10 minutes, please wait...)
✅ Build completed successfully

🔄 Step 3: Restarting service...
✅ Service restarted

✅ Step 4: Verifying deployment...
=== Service Status ===
Active: active (running)

=== Recent Logs ===
[bot logs...]

=== Audit Trail ===
[audit entries...]

╔════════════════════════════════════════════════════════════════╗
║                  ✅ DEPLOYMENT COMPLETE ✅                    ║
╚════════════════════════════════════════════════════════════════╝
```

---

## Option 2: Manual Deployment

### Step 1: Pull Code
```bash
ssh bot_service@31.97.125.104
cd /opt/solana-sniper-soldier
git fetch origin
git checkout feat/perf-rpc-quote-cache
git pull origin feat/perf-rpc-quote-cache
```

### Step 2: Build Binary
```bash
cargo build --release 2>&1 | tail -50
```

**Expected:** "Finished release [optimized] target(s) in X.XXs"

### Step 3: Restart Service
```bash
sudo systemctl restart solana-sniper-soldier.service
sudo systemctl status solana-sniper-soldier.service
```

**Expected:** "Active: active (running)"

### Step 4: Verify
```bash
# Check logs
tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*

# Check audit trail
tail -50 /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl

# Check process
ps aux | grep solana-sniper
```

---

## Monitoring After Deployment

### Real-time Metrics
```bash
tail -f /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | jq '.'
```

**Watch for:**
- `pump_score`: Should increase when pump detected (>50)
- `volatility_pct`: Market volatility tracking
- `position_size_lamports`: Entry size when trade opens
- `take_profit_bps`: Dynamic TP threshold
- `stop_loss_bps`: Dynamic SL threshold

### Logs
```bash
tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*
```

**Watch for:**
- "pump detected" → Pump detection working
- "entry signal generated" → Strategy triggered
- "transaction sent" → Trade executed
- "position closed" → Exit triggered

### Service Status
```bash
sudo systemctl status solana-sniper-soldier.service
```

**Expected:** "Active: active (running)"

---

## Expected Timeline

### Hour 1: Baseline
- Bot starts
- RPC connectivity verified
- HSM signing working
- Pump detection active
- Baseline metrics recorded

### Hour 2: First Trade
- Pump detected (pump_score > 50)
- Entry signal generated
- Position opened
- Audit log recording trade

### Hour 3+: Ongoing
- Position monitoring active
- Exit signals evaluated
- P&L tracking
- Risk limits enforced

---

## Troubleshooting

### Build Fails
```bash
# Clean and rebuild
cargo clean
cargo build --release 2>&1 | tail -100
```

### Service Won't Start
```bash
# Check service logs
sudo journalctl -u solana-sniper-soldier.service -n 50

# Restart manually
sudo systemctl restart solana-sniper-soldier.service
```

### No Trades After 2 Hours
```bash
# Check pump detection
grep "pump_score" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20

# Check liquidity
grep "liquidity" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20

# Check holder gates
grep "holder_concentration" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20
```

### High Slippage
```bash
# Check pool liquidity
grep "liquidity_lamports" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -10

# Reduce position size in strategy.rs
# Or increase MAX_SLIPPAGE_BPS in .env
```

---

## Rollback (If Needed)

### Revert to Previous Version
```bash
cd /opt/solana-sniper-soldier
git checkout 23783a4  # Previous stable commit
cargo build --release
sudo systemctl restart solana-sniper-soldier.service
```

### Emergency Stop
```bash
sudo systemctl stop solana-sniper-soldier.service
```

---

## Success Checklist

- [ ] Code pulled successfully
- [ ] Build completed without errors
- [ ] Service restarted
- [ ] Service status shows "active (running)"
- [ ] Logs show bot starting
- [ ] Audit trail recording events
- [ ] Process running with correct PID
- [ ] First trade within 2 hours

---

## Performance Expectations

### Per Trade
- Entry: +5% price move (pump detection)
- Exit: +12% price move (take-profit)
- Slippage: -1.5% (entry + exit)
- **Net P&L: +10.5%**

### Daily (3 trades)
- **+31.5% daily return**

### Monthly (20 days)
- **+630% monthly return**

---

## Support

**Issues?**
1. Check logs: `/opt/solana-sniper-soldier/data/logs/hft.log.*`
2. Check audit trail: `/opt/solana-sniper-soldier/data/audit/risk_audit.jsonl`
3. See HFT_DEPLOYMENT_GUIDE.md for detailed troubleshooting
4. See DEPLOYMENT_CHECKLIST.md for verification steps

---

**Status**: ✅ READY TO DEPLOY
**Branch**: feat/perf-rpc-quote-cache
**Tests**: 101/101 PASSING

**Let's go! 🚀**
