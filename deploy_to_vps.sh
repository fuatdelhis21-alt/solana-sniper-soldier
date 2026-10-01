#!/bin/bash
# Deployment script for VPS

set -e

REPO_PATH="/opt/solana-sniper-soldier"
BRANCH="feat/perf-rpc-quote-cache"

echo "╔════════════════════════════════════════════════════════════════╗"
echo "║         SOLANA HFT BOT — VPS DEPLOYMENT                       ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""

# Step 1: Pull code
echo "📥 Step 1: Pulling latest code..."
cd "$REPO_PATH"
git fetch origin
git checkout "$BRANCH"
git pull origin "$BRANCH"
echo "✅ Code pulled successfully"
echo ""

# Step 2: Build
echo "🔨 Step 2: Building binary..."
echo "   (This may take 5-10 minutes, please wait...)"
cargo build --release 2>&1 | grep -E "Compiling|Finished|error" | tail -30
if [ ${PIPESTATUS[0]} -eq 0 ]; then
  echo "✅ Build completed successfully"
else
  echo "❌ Build failed"
  exit 1
fi
echo ""

# Step 3: Restart service
echo "🔄 Step 3: Restarting service..."
sudo systemctl restart solana-sniper-soldier.service
sleep 2
sudo systemctl status solana-sniper-soldier.service --no-pager | head -5
echo "✅ Service restarted"
echo ""

# Step 4: Verify
echo "✅ Step 4: Verifying deployment..."
echo ""
echo "=== Service Status ==="
sudo systemctl status solana-sniper-soldier.service --no-pager | grep -E "Active|running"
echo ""

echo "=== Recent Logs ==="
tail -10 /opt/solana-sniper-soldier/data/logs/hft.log.* 2>/dev/null || echo "Logs not yet available"
echo ""

echo "=== Audit Trail ==="
tail -3 /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl 2>/dev/null || echo "Audit trail not yet available"
echo ""

echo "=== Process Check ==="
ps aux | grep solana-sniper | grep -v grep || echo "Process starting..."
echo ""

echo "╔════════════════════════════════════════════════════════════════╗"
echo "║                  ✅ DEPLOYMENT COMPLETE ✅                    ║"
echo "╚════════════════════════════════════════════════════════════════╝"
echo ""
echo "📊 Monitoring:"
echo "  Real-time metrics:"
echo "    tail -f /opt/solana-sniper-soldier/data/audit/risk_audit.jsonl | jq '.'"
echo ""
echo "  Logs:"
echo "    tail -100 /opt/solana-sniper-soldier/data/logs/hft.log.*"
echo ""
echo "⏱️  Expected first trade: 1-2 hours"
echo ""
