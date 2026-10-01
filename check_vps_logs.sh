#!/bin/bash
echo "=== VPS Audit Log (son 100 satır) ==="
ssh -o ConnectTimeout=10 bot_service@31.97.125.104 'tail -100 /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl 2>/dev/null' 2>&1 | tail -50

echo ""
echo "=== VPS HFT Log (son 50 satır) ==="
ssh -o ConnectTimeout=10 bot_service@31.97.125.104 'tail -50 /opt/solana-sniper-soldier/data/logs/hft.log.* 2>/dev/null | tail -30' 2>&1
