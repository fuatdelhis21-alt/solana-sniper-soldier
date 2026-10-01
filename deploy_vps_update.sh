#!/bin/bash
set -euo pipefail

VPS_IP="31.97.125.104"
VPS_USER="bot_service"
REPO_PATH="/opt/solana-sniper-soldier"
BRANCH="feat/perf-rpc-quote-cache"

echo "=== Pulling latest code on VPS ==="
ssh "$VPS_USER@$VPS_IP" "cd $REPO_PATH && git fetch origin && git checkout $BRANCH && git pull origin $BRANCH" || exit 1

echo "=== Building on VPS (this may take 5-10 minutes) ==="
ssh "$VPS_USER@$VPS_IP" "cd $REPO_PATH && timeout 600 cargo build --release 2>&1 | grep -E 'Compiling|Finished|error' | tail -30" || exit 1

echo "=== Restarting bot service ==="
ssh "$VPS_USER@$VPS_IP" "sudo systemctl restart solana-sniper-soldier.service" || exit 1

echo "=== Checking service status ==="
ssh "$VPS_USER@$VPS_IP" "sudo systemctl status solana-sniper-soldier.service --no-pager | head -20" || exit 1

echo "✓ Deployment complete"
