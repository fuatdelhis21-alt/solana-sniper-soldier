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
| `/ready` 503, `reconcile_required:1` | Operatör on-chain state'i doğrular, `clear_reconcile_required` sonrası restart. Otomatik satış YOK. |
| `/ready` 503, `exit_blocked:1` | Exit retry bütçesi tükendi; pozisyon açık. Operatör müdahalesi gerekir. |
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
