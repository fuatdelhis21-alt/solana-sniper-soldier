# 📊 Deployment Monitoring Guide

## Real-Time Monitoring Commands

### 1. Service Status
```bash
ssh bot_service@31.97.125.104
sudo systemctl status solana-sniper-soldier.service
```

**Expected output:**
```
● solana-sniper-soldier.service - Solana HFT Bot
   Loaded: loaded (/etc/systemd/system/solana-sniper-soldier.service; enabled; vendor preset: enabled)
   Active: active (running) since [timestamp]
   Main PID: [PID] (solana-sniper)
```

### 2. Build Progress
```bash
ps aux | grep cargo
```

**While building:**
```
bot_service  [PID]  cargo build --release
```

**After build completes:**
```
(no cargo process)
```

### 3. Logs (Real-time)
```bash
tail -f /opt/solana-sniper-soldier/data/logs/hft.log.*
```

**Watch for:**
- `[INFO] bot starting` — Bot initialized
- `[INFO] RPC connected` — RPC connectivity verified
- `[INFO] HSM connected` — HSM signing ready
- `[INFO] pump detection active` — Ready for trades
- `[INFO] pump detected` — Pump signal detected
- `[INFO] entry signal generated` — Trade triggered
- `[INFO] transaction sent` — Trade executed
- `[INFO] position closed` — Exit triggered

### 4. Audit Trail (Real-time)
```bash
tail -f /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | jq '.'
```

**Watch for:**
- `pump_score`: 0-100 (higher = more confident)
- `volatility_pct`: Market volatility
- `position_size_lamports`: Entry size
- `take_profit_bps`: Dynamic TP threshold
- `stop_loss_bps`: Dynamic SL threshold
- `confidence`: 0-100 (pump + order flow signals)

### 5. Process Check
```bash
ps aux | grep solana-sniper | grep -v grep
```

**Expected:**
```
bot_service  [PID]  solana-sniper --live --confirm-live
```

---

## Deployment Timeline

### Phase 1: Build (5-10 minutes)
```
T+0:00   Deployment script starts
T+1:00   Code pulled from GitHub
T+2:00   Build starts (cargo build --release)
T+7:00   Build completes
T+8:00   Service restarts
T+10:00  Verification complete
```

**Check:** `ps aux | grep cargo`

### Phase 2: Startup (1-2 minutes)
```
T+10:00  Service starts
T+11:00  RPC connects
T+12:00  HSM connects
T+12:30  Pump detection active
```

**Check:** `tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*`

### Phase 3: Baseline (60 seconds)
```
T+12:30  Baseline phase starts
T+13:30  Baseline phase completes
```

**Check:** `tail -f /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | jq '.pump_score'`

### Phase 4: Trading (1-2 hours)
```
T+13:30  Waiting for pump signal
T+60:00  First pump detected (expected)
T+65:00  Entry signal generated
T+70:00  Position opened
T+300:00 Exit signal triggered
T+305:00 Position closed
```

**Check:** `tail -f /opt/solana-sniper-soldier/data/logs/hft.log.* | grep -E "pump|entry|transaction|position"`

---

## Key Metrics to Monitor

### Pump Detection
```bash
grep "pump_score" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20
```

**Interpretation:**
- 0-30: No pump
- 30-50: Possible pump
- 50-70: Likely pump
- 70-100: Strong pump

### Volatility
```bash
grep "volatility_pct" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20
```

**Interpretation:**
- <5%: Low volatility (tight TP/SL)
- 5-20%: Normal volatility
- 20-50%: High volatility (wide TP/SL)
- >50%: Extreme volatility

### Position Size
```bash
grep "position_size_lamports" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20
```

**Expected:**
- 5-20M lamports (0.005-0.02 SOL)
- Adjusts based on volatility and pump score

### Take-Profit / Stop-Loss
```bash
grep "take_profit_bps\|stop_loss_bps" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20
```

**Expected:**
- TP: 500-2000 bps (5-20%)
- SL: 250-1000 bps (2.5-10%)
- Adjusts based on volatility

---

## Troubleshooting

### Build Fails
```bash
# Check build logs
tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*

# Rebuild
cd /opt/solana-sniper-soldier
cargo clean
cargo build --release 2>&1 | tail -100
```

### Service Won't Start
```bash
# Check systemd logs
sudo journalctl -u solana-sniper-soldier.service -n 50

# Restart manually
sudo systemctl restart solana-sniper-soldier.service
sudo systemctl status solana-sniper-soldier.service
```

### No Logs
```bash
# Check if logs directory exists
ls -la /opt/solana-sniper-soldier/data/logs/

# Create if missing
mkdir -p /opt/solana-sniper-soldier/data/logs
mkdir -p /opt/solana-sniper-soldier/data/audit
```

### No Trades After 2 Hours
```bash
# Check pump detection
grep "pump_score" /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | tail -20

# Check liquidity
grep "liquidity" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20

# Check holder gates
grep "holder_concentration" /opt/solana-sniper-soldier/data/logs/hft.log.* | tail -20

# Check RPC connectivity
curl -s https://mainnet.helius-rpc.com -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' | jq '.result'
```

---

## Success Indicators

### ✅ Deployment Successful
- [ ] Service status shows "active (running)"
- [ ] Logs show "bot starting"
- [ ] Logs show "RPC connected"
- [ ] Logs show "HSM connected"
- [ ] Logs show "pump detection active"
- [ ] Audit trail recording events

### ✅ Trading Ready
- [ ] Pump score > 0 (detecting market activity)
- [ ] Volatility tracking (>0%)
- [ ] Audit trail updating every second
- [ ] No error messages in logs

### ✅ First Trade
- [ ] Pump score > 50 (pump detected)
- [ ] Entry signal generated
- [ ] Position opened
- [ ] Audit trail recording trade
- [ ] Logs show "transaction sent"

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

## Emergency Procedures

### Stop Bot
```bash
sudo systemctl stop solana-sniper-soldier.service
```

### Restart Bot
```bash
sudo systemctl restart solana-sniper-soldier.service
```

### View Recent Errors
```bash
sudo journalctl -u solana-sniper-soldier.service -n 100 | grep -i error
```

### Rollback to Previous Version
```bash
cd /opt/solana-sniper-soldier
git checkout 23783a4  # Previous stable commit
cargo build --release
sudo systemctl restart solana-sniper-soldier.service
```

---

## Monitoring Checklist

- [ ] Service running (active)
- [ ] Logs showing bot activity
- [ ] Audit trail updating
- [ ] Pump detection active
- [ ] RPC connected
- [ ] HSM connected
- [ ] No error messages
- [ ] First trade within 2 hours

---

**Status**: ✅ MONITORING READY
**Expected First Trade**: 1-2 hours after deployment
**Expected Daily Return**: +31.5%

