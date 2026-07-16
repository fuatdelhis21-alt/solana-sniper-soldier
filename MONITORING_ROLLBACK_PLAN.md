# Solana HFT Platform — Monitoring & Rollback Strategy

## Monitoring Setup

### 1. Prometheus Metrics Endpoint
```toml
# config/production.example.toml
[telemetry]
json_logs = true
log_level = "info"
prometheus_port = 9090
```

### 2. Scraped Metrics (Expected)
```
# Counter examples (from hft-core, hft-marketdata, etc.)
hft_market_events_total{source="geyser"}
hft_pipeline_dedup_dropped
hft_geyser_reconnects_total
hft_execution_orders_total{status="success|failed"}
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
# If issues detected post-deploy:
systemctl stop solana-hft

# Restore previous version:
rm -rf /opt/solana-hft/*
tar -xzf /tmp/solana-hft-backup-TIMESTAMP.tar.gz \
  -C /opt/solana-hft

# Restart:
systemctl start solana-hft
systemctl status solana-hft

# Verify:
curl http://localhost:9090/metrics | grep hft_uptime_seconds
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
```toml
# config/production.example.toml
[execution]
mode = "shadow"  # → Log orders but don't execute
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
        annotations:
          summary: "Solana HFT Platform is down"

      - alert: HFTHighErrorRate
        expr: rate(hft_errors_total[5m]) > 0.001
        for: 5m
        annotations:
          summary: "High error rate (> 0.1%)"

      - alert: HFTHighLatency
        expr: hft_execution_latency_p99_ms > 50
        for: 5m
        annotations:
          summary: "P99 latency > 50ms"
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
