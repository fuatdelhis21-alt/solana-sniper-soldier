# VPS Operasyonel Runbook — solana-hft-platform

Bu runbook, VPS üzerinde bot + HSM servislerinin güvenli başlatma, durdurma,
izleme ve kurtarma prosedürlerini açıklar. **Mainnet işlemi YASAKTIR.** Tüm
komutlar devnet/test ortamı içindir.

---

## 1. Servis Durumu

```bash
# Her iki servisin durumu
systemctl status solana-sniper solana-hsm

# Sadece aktif mi?
systemctl is-active solana-sniper solana-hsm

# Son loglar
journalctl -u solana-sniper -n 100 --no-pager
journalctl -u solana-hsm -n 100 --no-pager
```

---

## 2. Başlatma (Paper/Dry-Run — Varsayılan)

Varsayılan mod **paper**'dır (on-chain işlem yok, HSM gerekmez).

```bash
# Paper modda başlat (varsayılan unit ExecStart)
sudo systemctl start solana-sniper

# Dry-run'a geçmek için unit'i düzenle (--paper -> --dry-run), sonra:
sudo systemctl daemon-reload
sudo systemctl restart solana-sniper
```

**Doğrulama:**
```bash
# Process ayakta mı? (liveness)
curl -s http://127.0.0.1:9898/health   # -> "ok"

# Trade-ready mi? (readiness — paper/dry-run)
curl -s http://127.0.0.1:9898/ready     # -> {"ready":true,...} veya 503
```

`/ready` 503 dönüyorsa bot trade-ready DEĞİLDİR — process ayakta olsa bile.
503'ün nedenini JSON gövdesinden oku (örn. `reconcile_required:true`).

---

## 3. Durdurma (Güvenli Kapanma)

```bash
# Graceful stop: systemd SIGTERM gönderir; bot state'i persist eder.
sudo systemctl stop solana-sniper

# HSM'i de durdur
sudo systemctl stop solana-hsm
```

**Kritik:** Açık pozisyon varsa, durdurmadan önce state'in persist edildiğini
doğrula:
```bash
# risk_state.json mevcut ve geçerli mi?
sudo -u solana-bot test -f /var/lib/solana-hft/risk_state.json && echo "state present"
```

---

## 4. Log Kontrolü

```bash
# JSON structured loglar (günlük rotation)
journalctl -u solana-sniper --since "10 min ago" --no-pager

# Audit logu (risk olayları)
sudo -u solana-bot tail -n 50 /var/lib/solana-hft/audit/risk_audit.jsonl

# HSM audit logu
sudo -u solana-hsm tail -n 50 /var/log/solana-hft-hsm/hsm_audit.log
```

**Loglarda secret arama (olmamalı):**
```bash
journalctl -u solana-sniper --no-pager | grep -iE "private key|HSM_KEY|BEGIN|secret" || echo "temiz"
```

---

## 5. Metrics Kontrolü

```bash
# Tüm metrikler
curl -s http://127.0.0.1:9898/metrics

# Kritik durumlar
curl -s http://127.0.0.1:9898/metrics | grep -E "breaker_state|exit_blocked|reconcile_required|kill_switch_state"
```

| Metrik | Sağlıklı değer |
|--------|----------------|
| `breaker_state` | 0 (closed) |
| `exit_blocked` | 0 |
| `reconcile_required` | 0 |
| `kill_switch_state{reason="integrity"}` | 0 |
| `kill_switch_state{reason="risk_limit"}` | 0 |

---

## 6. Breaker Kontrolü

```bash
# Breaker durumu (0=closed, 1=half_open, 2=open)
curl -s http://127.0.0.1:9898/metrics | grep "^breaker_state"

# Open ise: cooldown sonrası bot salt-okunur probe çalıştırır.
# Probe başarılıysa HalfOpen'a geçer; başarısızsa Open kalır.
# Breaker asla zamanla otomatik kapanmaz (fail-closed).
```

---

## 7. State Kontrolü

```bash
# State dosyası geçerli mi? (bozuk ise live startup reddedilir)
sudo -u solana-bot python3 -c "import json;json.load(open('/var/lib/solana-hft/risk_state.json'));print('valid')"

# Açık pozisyon var mı?
curl -s http://127.0.0.1:9898/metrics | grep "^open_position_count"
```

---

## 8. HSM Başlatma (Live için — Manuel Onay Gerekir)

> **Live mod yalnızca operatörün açık manuel onayıyla ve devnet/test ortamında
> yapılmalıdır. Mainnet live YASAKTIR.**

```bash
# 1. HSM'i başlat (private key /etc/solana-hft-hsm/hsm.env'den okunur)
sudo systemctl start solana-hsm

# 2. HSM health + pubkey doğrula
curl -s --cert /etc/solana-hft/hsm/client_all.pem \
     --cacert /etc/solana-hft/hsm/ca.pem \
     https://127.0.0.1:8443/pubkey

# 3. Bot'u live modda başlat (--live --confirm-live + HSM mTLS)
#    Unit ExecStart'ı düzenle, sonra:
sudo systemctl daemon-reload
sudo systemctl restart solana-sniper

# 4. Live readiness doğrula
curl -s http://127.0.0.1:9898/ready   # -> ready:true (200)
```

---

## 9. Kurtarma Senaryoları

| Durum | Eylem |
|-------|-------|
| `exit_blocked:1` / alert `SolanaHftExitBlocked` (≥ 3 ardışık başarısız exit) | Pozisyon açık; bot her iterasyonda exit'i **yeniden dener** (günlük limit / kill switch exit'i engellemez). Bakılacaklar: `journalctl` içinde `exit iteration failed` hatası (RPC, HSM, `exit quote failed`, havuz likiditesi). RPC/HSM'i düzelt; servisi **durdurma** (durdurursan pozisyon `live_position.json`'da kalır, sonraki süreç yönetimi devralır). Kayma toleransı otomatik genişlemez (`SNIPER_EXIT_SLIPPAGE_ESCALATION=false`); havuz çok inceldiyse operatör bilinçli olarak açabilir. Manuel satış yapılırsa bot token'ı cüzdanda bulamaz ve kaydı `ClosedExternally` olarak kapatır. |
| Kill switch (`hft_kill_switch_active 1`) | Yeni giriş yok, exit devam eder. Tetikleyiciyi audit logda bul (ör. "entry outcome unknown" → cüzdanı explorer'da kontrol et, `pending_entry.json` bir sonraki başlangıçta zincirden uzlaştırılır). |
| `state_verified 0` | risk_state.json okunamadı ya da yazılamadı (disk dolu / izin). Yeni giriş yok. Disk/izni düzelt, dosyayı incele, sonra restart. |
| `another trading process holds …/trading.lock` | Aynı data dir için ikinci live süreç. İkinci servisi durdur (`solana-hft` ve `solana-hft-sniper` aynı anda çalışmaz). |
| Log `CLMM log stream DOWN` (ERROR, every 10 min) / `logsSubscribe failed … 429` | The RPC/WS provider refuses requests (quota/plan). Discovery and paper stop (fail-closed, no trades). Check with the commands under "Provider quota" below; fix the plan/key or switch `SNIPER_RPC` / `SNIPER_WS`, then `systemctl restart solana-hft-sniper`. |
| pool-sniper `/ready` 503 ("possible discovery gap") | WS yeniden bağlandı / kuyruk taştı / CreatePool çözülemedi: o pencerede yeni havuz kaçmış olabilir (backfill yok). 5 dk sonra kendiliğinden düzelir; sürekli tekrarlıyorsa WS/RPC sağlayıcısını kontrol et. |
| pool-sniper "corrupt discovery state" ile başlamıyor | `/var/lib/solana-hft/discovery_state.json` bozuk: incele, sil (günlük havuz sayacı sıfırlanır — bilinçli karar). |
| Breaker Open | Cooldown + probe akışını bekle; probe başarısızsa RPC/HSM'i kontrol et. |
| HSM down | `systemctl start solana-hsm`; bot live'da pubkey'i yeniden doğrular. |
| risk_state.json bozuk | Live startup reddedilir. Yedekten restore etme otomatik değildir; operatör doğrular. |
| Restart loop | `systemctl status` ile `StartLimitBurst` aşımını kontrol et; logları incele. |

---

## 10. Güvenli Kapanma Prosedürü

```bash
# 1. Bot'u durdur (state persist edilir)
sudo systemctl stop solana-sniper

# 2. HSM'i durdur
sudo systemctl stop solana-hsm

# 3. State'in persist edildiğini doğrula
sudo -u solana-bot test -f /var/lib/solana-hft/risk_state.json && echo "state persisted"

# 4. (Opsiyonel) Servisleri devre dışı bırak
sudo systemctl disable solana-sniper solana-hsm
```

---

## 11. Provider quota (HTTP 429)

The bot's JSON logs all reach journald at priority "info", so `journalctl
-p warning` does not show them: filter on the JSON level instead.

```bash
journalctl -u solana-hft-sniper --since "1 hour ago" | grep -E '"level":"(ERROR|WARN)"' | tail -20
set -a; . /etc/solana-hft/solana-hft.env; set +a      # nothing is printed
curl -s -o /dev/null -w "getSlot http=%{http_code}\n" -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}' "$SNIPER_RPC"
curl -s -o /dev/null --max-time 5 --http1.1 -w "ws handshake=%{http_code}\n" \
  -H "Connection: Upgrade" -H "Upgrade: websocket" -H "Sec-WebSocket-Version: 13" \
  -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" "${SNIPER_WS/wss:/https:}"
```

`200` / `101` = fine, `429` = quota or rate limit, `401`/`403` = key or plan.

## 12. Collecting real measurements (operator)

Nothing below sends a transaction.

```bash
# Latency per stage (histogram) and readiness. The bot's /metrics exists
# only while a trading process runs (SNIPER_METRICS_ADDR).
curl -s 127.0.0.1:9091/metrics | grep -E "ready|gap|ws_last|reconnects"
curl -s 127.0.0.1:9090/metrics | grep -E "sniper_stage_latency_ms_(sum|count)|hft_hsm_latency_ms_(sum|count)"
# Paper performance, MFE/MAE, stress case, latency percentiles from the records
python3 /opt/solana-hft/sniper_report.py /var/lib/solana-hft --hours 72
# RPC calls actually made: the provider dashboard, a day before and after
# deploying the RPC-reduction build.
```

One-off **dry-run on a launch pool** (never broadcasts; the exit is also
simulated as an existing token holder). Use a pool pool-sniper reported
(`new wSOL pool on watch list`), a separate data dir, and the signal gate
off only for this check (otherwise the ~70 s warm-up stops it before the
quote):

```bash
sudo -u solana bash -c 'set -a; . /etc/solana-hft/solana-hft.env; set +a
  SNIPER_DATA_DIR=/tmp/dryrun-check /opt/solana-hft/bin/solana-sniper --dry-run \
    --pool-id <POOL> --input-mint So11111111111111111111111111111111111111112 \
    --output-mint <TOKEN> --iterations 3 --signal-gate false' 2>&1 \
  | grep -E "exact entry quote|EXIT simulation|rejected|error" | sed -E 's/(api-key=)[^"&]*/\1***/g'
```

Expected: `exact entry quote (on-chain simulation)` and
`EXIT simulation as token holder <wallet>: OK`. A rejection (liquidity,
authority, edge, impact) is also a valid, safe outcome — paste it.
