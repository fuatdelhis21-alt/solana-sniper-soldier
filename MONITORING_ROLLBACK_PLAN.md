# Solana HFT Platform — Monitoring & Rollback Strategy

## Monitoring Setup

### 1. Prometheus Metrics Endpoint
Bot, `SNIPER_METRICS_ADDR` (systemd varsayılanı `127.0.0.1:9090`) üzerinde
`GET /metrics` ve `GET /health` sunar. Adres bağlanamazsa bot başlamaz.

### 2. Metrikler (gerçek isimler — `solana-sniper/src/metrics.rs`)
```
trade_attempt_total{mode}                 risk-gate değerlendirmeleri
trade_rejected_total{mode,reason}         RejectReason kodlarıyla red
trade_executed_total{mode}                gönderilip onaylanan (live) işlemler
circuit_breaker_state                     1 = açık
hft_kill_switch_active                    1 = aktif
daily_realized_pnl                        günlük gerçekleşen zarar (lamports, negatif)
open_position_count                       açık pozisyon sayısı
hft_trades_total / hft_trades_success / hft_trades_failed
hft_trade_latency_ms                      histogram
hft_hsm_requests / hft_hsm_errors / hft_hsm_latency_ms
```

### 3. Health Check (HTTP)
```bash
# On-machine check:
curl -s http://localhost:9090/health || echo "UNHEALTHY"

# Alert if: 
# - Response time > 50ms
# - Process not running (systemd status)
# - Log error spike (journalctl pattern match)
```

---

## Rollback Procedure

### Pre-Deployment Backup
```bash
# On target before deploy:
tar -czf /tmp/solana-hft-backup-$(date +%Y%m%d-%H%M%S).tar.gz \
  -C /opt/solana-hft . 2>/dev/null || echo "No prior install"
```

### Instant Rollback (< 30 seconds)
```bash
# install_release.sh önceki binary'leri /opt/solana-hft/bin.prev'e saklar:
sudo /opt/solana-hft/rollback.sh
curl -s http://localhost:9090/health
```

### Full Rollback (Config + Binaries)
```bash
# If config also needs rollback:
cp /etc/solana-hft/config.toml /etc/solana-hft/config.toml.failed
cp /tmp/solana-hft-config-backup-TIMESTAMP.toml /etc/solana-hft/config.toml
systemctl restart solana-hft
```

---

## Canary Deployment (Optional)

### Shadow Mode (No Real Orders)
```bash
# /etc/solana-hft/solana-hft.env
SNIPER_DRY_RUN=true   # gerçek swap tx kurulur + imzalanır, asla gönderilmez
```

### Gradual Rollout
1. Deploy to 1 node in shadow mode (24h)
2. Verify metrics stable → market_events OK
3. Deploy to 2nd node (shadow)
4. Switch execution mode to "live"
5. Monitor error rate < 0.1% for 1h

---

## Alert Rules (Prometheus/AlertManager)

```yaml
groups:
  - name: hft-platform
    rules:
      - alert: HFTPlatformDown
        expr: up{job="solana-hft"} == 0
        for: 2m
      - alert: HFTCircuitBreakerOpen
        expr: circuit_breaker_state == 1
        for: 5m
      - alert: HFTKillSwitchActive
        expr: hft_kill_switch_active == 1
      - alert: HFTHsmErrors
        expr: increase(hft_hsm_errors[10m]) > 0
      - alert: HFTTradeFailures
        expr: increase(hft_trades_failed[15m]) > 2
```

---

## Incident Response Flowchart

```
Metric Alert Triggered?
├─ YES → Check logs: journalctl -u solana-hft --since 5m
├─ Error patterns? → Decision tree
│   ├─ Config error → Fix config, restart
│   ├─ Code bug → Rollback to previous version
│   ├─ Upstream issue (Geyser/RPC down) → Wait + monitor
│   └─ Unknown → Escalate, preserve logs
└─ NO → Investigate prometheus scrape job, network connectivity
```

---

## Post-Deploy Validation Checklist

- [ ] Service running: `systemctl is-active solana-hft` → active
- [ ] Process UP: `ps aux | grep solana-hft`
- [ ] Metrics exposed: `curl http://localhost:9090/metrics` → 200
- [ ] Logs clean: `journalctl -u solana-hft -n 50` → no ERRORs
- [ ] CPU/Memory: `top -p $(pgrep -f solana-hft)` → reasonable baseline
- [ ] Network: `netstat -tlnp | grep solana` → listening on expected ports

---

## Estimated Recovery Times (RTO)

| Scenario | Time | Method |
|----------|------|--------|
| Config restart | < 5s | `systemctl restart` |
| Rollback to prev version | < 30s | tar restore + start |
| Full redeploy | < 2m | Ansible playbook |
| OS-level restore | < 10m | Snapshot/image restore |

---

## Backup Schedule (Recommended)

```bash
# Daily backup (cron)
0 2 * * * tar -czf /backups/solana-hft-$(date +\%Y\%m\%d).tar.gz -C /opt/solana-hft . 2>&1 | logger -t hft-backup

# Retention: Keep last 7 days + monthly archives
find /backups -name "solana-hft-*.tar.gz" -mtime +7 -delete
```

---

**Status**: Monitoring strategy outlined | Rollback < 30s guaranteed | Post-deploy validation checklist ready
