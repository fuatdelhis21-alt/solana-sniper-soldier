# VPS + 7/24 Operasyonel Altyapı — Mimari Dokümanı

Bu doküman, `solana-hft-platform`'un güvenli ve minimum yetkili bir VPS üzerinde
7/24 çalışması için tasarımı açıklar. **Mainnet işlemi YASAKTIR**; bu doküman
yalnızca deployment tasarımı, systemd yapılandırması, HSM/mTLS operasyonu,
health/readiness, restart güvenliği, alert ve paper/dry-run hazırlığı kapsar.

---

## 1. Mimari Genel Bakış (BÖLÜM 2)

Üç ayrı süreç/servis, üç ayrı kullanıcı, minimum yetki:

```
                    ┌─────────────────────────────────────────────┐
                    │  VPS (Linux)                                │
                    │                                             │
  Prometheus ─────► │  solana-sniper.service   (user: solana-bot) │
  (localhost)       │    paper/dry-run DEFAULT                    │
                    │    live = manuel arm + --confirm-live       │
                    │    metrics 127.0.0.1:9898                   │
                    │    /health (liveness)  /ready (readiness)   │
                    │         │  mTLS (loopback)                  │
                    │         ▼                                   │
                    │  solana-hsm.service      (user: solana-hsm) │
                    │    hsm_server 127.0.0.1:8443                │
                    │    private key ONLY here (HSM_KEY_B64)      │
                    │                                             │
                    │  RPC/WS (devnet/test) ◄── bot               │
                    └─────────────────────────────────────────────┘
```

### A. Bot process (`solana-sniper`)
- Rust binary. **Varsayılan mod paper veya dry-run** — on-chain işlem yok.
- Live mod, açıkça arm edilmeden çalışmaz: `--live --confirm-live` + `arm_live`
  gerekir (fail-closed). Mainnet endpoint varsayılan değildir.
- Mainnet kullanımı için mevcut `confirm_live`, arm switch ve tüm fail-closed
  kontroller korunur.

### B. HSM signing process (`hsm_server`)
- Ayrı servis, ayrı kullanıcı (`solana-hsm`). Private key bot process'ine
  **verilmez**.
- Bot yalnızca mTLS üzerinden signing request gönderir (`POST /sign`).
- HSM health check + pubkey doğrulaması yapılır. HSM yoksa bot live mode'a
  geçmez. HSM secret/key içeriği loglanmaz.
- HSM restart sonrası bot, pubkey + health'i yeniden doğrular.

### C. Metrics
- `spawn_metrics_server` default `127.0.0.1:9898` (localhost) — kaynak koddan
  doğrulandı. Metrics portu internete açılmaz.
- VPS üzerinde localhost'a bağlı Prometheus scrape yapılandırması önerilir.
- `0.0.0.0` bind yalnızca açık ve gerekçeli kullanıcı yapılandırmasıyla mümkün;
  varsayılan değildir.

### D. Loglar
- Structured JSON log formatı (mevcut `init_tracing`), günlük dosya rotation
  (`tracing_appender::rolling::daily`).
- Secret, private key, token, mTLS credential, HSM payload veya tam imza
  içeriği yazılmaz.
- Disk dolması durumunda: log yazma hatası audit edilir; risk state persist
  hatası `state_persist_failed` audit'i ile raporlanır (fail-closed).
- Kritik hata logları audit edilebilir (risk audit logu).

---

## 2. Kullanıcı / İzin Modeli (BÖLÜM 3)

| Servis | Kullanıcı | Grup | Okunabilir | Yazılabilir |
|--------|-----------|------|------------|-------------|
| `solana-sniper` | `solana-bot` | `solana-bot` | binary, config, mTLS client identity | `/var/lib/solana-hft`, `/var/log/solana-hft` |
| `solana-hsm` | `solana-hsm` | `solana-hsm` | binary, certs, `HSM_KEY_B64` | `/var/log/solana-hft-hsm` |

- `solana-bot` **HSM private key'e erişemez** (dosya `solana-hsm` sahipli, 0600).
- `solana-hsm` mTLS cert/key dosyalarını yalnızca kendisi okur.
- Her iki servis de `NoNewPrivileges=true`, boş `CapabilityBoundingSet`,
  `ProtectSystem=strict`, `ProtectHome=true`, `PrivateTmp=true`.

---

## 3. Health vs Readiness Ayrımı (BÖLÜM 5)

**Process'in ayakta olması trade-ready anlamına gelmez.** Üç ayrı katman:

| Katman | Endpoint / Sinyal | Anlamı |
|--------|-------------------|--------|
| **Process alive** | `GET /health` → 200 "ok" | Process çalışıyor, metrics server ayakta. |
| **Health** | `/ready` + gauges | Risk state okunabiliyor, metrics cevap veriyor, HSM/RPC durumu biliniyor. |
| **Readiness (paper/dry-run)** | `/ready` → `ready_paper_dry_run()` | Market data alınabiliyor, state doğrulanıyor, paper işlemi simüle edilebilir. |
| **Readiness (live)** | `/ready` → `ready_live()` | Live arm açık, confirm-live mevcut, HSM mTLS erişilebilir + pubkey doğrulanmış, risk state doğrulanmış, RPC/pool güncel, breaker uygun, reconcile=0, EXIT_BLOCKED=0, metrics hazır. |

`/ready` endpoint'i `ReadinessSnapshot`'tan okur (main.rs her iterasyonda
`set_readiness` ile günceller). Live modda tüm fail-closed kapılar sağlanmadan
`ready_live()` false döner → `/ready` 503.

---

## 4. Restart / State Güvenliği (BÖLÜM 6)

`risk_state.json` **atomik yazılır** (`.tmp` + rename): çökme sırasında yarım
yazılmış dosya üretilmez. Senaryolar:

| Senaryo | Davranış |
|---------|----------|
| Normal kapanma | State persist edilir; restart'ta doğrulanır. |
| Ani çökme | Atomik write sayesinde ya eski ya yeni state; bozuk dosya yok. |
| VPS reboot | systemd `Restart=on-failure`; state doğrulanır. |
| HSM restart | Bot live'da HSM pubkey + health'i yeniden doğrular. |
| RPC geçici erişilemez | Breaker Open → yeni entry yok; exit fail-closed. |
| metrics portu dolu | Bind hatası fail-loud → process exit (observability'siz çalışmaz). |
| `risk_state.json` bozuk | `is_state_verified=false` → live startup reddedilir. |
| Açık pozisyon | Restart'ta korunur; kapatılmış işaretlenmez. |
| RECONCILE_REQUIRED | Yeni entry engellenir; otomatik satış yok. |
| EXIT_BLOCKED | Restart sonrası korunur; pozisyon açık kalır. |
| Breaker Open | Restart sonrası güvenli biçimde korunur (fail-closed). |

Backup restore otomatik ve körlemesine yapılmaz; operatör müdahalesi gereken
durumlar audit + `/ready` 503 ile açıkça raporlanır.

---

## 5. HSM / mTLS Operasyon Güvenliği (BÖLÜM 7)

- HSM process health: `GET /pubkey` (mTLS) — bot live başlangıcında çağırır.
- Pubkey discovery/verification: bot, beklenen pubkey'i doğrular; mismatch →
  live reddedilir.
- mTLS: `--hsm-ca` + `--hsm-client-identity` (PEM). Sunucu client cert ister
  (fail-closed).
- Certificate expiration: operasyonel kontrol (runbook).
- Certificate rotation: `generate_certs.ps1` (devnet/test) — production için
  ayrı prosedür.
- HSM restart sonrası bot davranışı: pubkey + health yeniden doğrulanır.
- Wrong pubkey / signing timeout / duplicate request / HSM unavailable:
  hepsi fail-closed — live işlem açılmaz, breaker Open'a gider.
- Bot private key'e erişemez (dosya izinleri).
- Signing request logları secret içermez (yalnızca event + reason).

---

## 6. Alert / Operatör Sinyalleri (BÖLÜM 8)

Aşağıdaki durumlar metrics gauge'ları + audit loglarıyla izlenir (secret'sız
label'lar):

| Durum | Metrik / Sinyal |
|-------|-----------------|
| HSM unavailable | `hft_hsm_errors`, breaker Open, `/ready` 503 |
| HSM pubkey mismatch | live startup reddi (audit) |
| RPC unavailable | breaker Open (`breaker_state=2`) |
| stale price | `trade_rejected_total{reason="stale_price"}` |
| circuit breaker Open | `breaker_state 2` |
| circuit breaker HalfOpen | `breaker_state 1` |
| EXIT_BLOCKED | `exit_blocked 1` |
| RECONCILE_REQUIRED | `reconcile_required 1` |
| risk state invalid | `state_verified=false` → `/ready` 503 |
| metrics bind failure | process exit (fail-loud) |
| repeated restart loop | systemd `StartLimitBurst` |
| daily loss limit | kill switch `risk_limit` |
| entry cap reached | kill switch `integrity` |
| successful exit | `trade_exit_executed_total` |
| failed exit | `trade_exit_rejected_total{reason=...}` |
| state persistence failure | `state_persist_failed` audit |

---

## 7. Paper / Dry-Run Operasyon Modu (BÖLÜM 9)

- Paper/dry-run gerçek transaction broadcast etmez, HSM signing kullanmaz
  (bu açıkça raporlanır).
- Gerçek piyasa verisi ile simulated decision akışı ayrılır.
- Entry/exit sinyalleri ve reddedilme nedenleri ölçülür.
- Simulated P&L ile realized on-chain P&L aynı metrik gibi sunulmaz.
- Paper mode live mode'a yanlışlıkla geçmez (`conflicts_with`).
- Paper mode'da risk gate'leri bypass edilmez; aynı gate'ler çalışır.
- Dry-run çıktısında private key/secret yoktur.
- Operasyonel runbook: `docs/VPS_RUNBOOK.md`.

---

## 8. Deployment Dosyaları

| Dosya | Amaç |
|-------|------|
| `deploy/systemd/solana-sniper.service` | Bot systemd unit (paper/dry-run default) |
| `deploy/systemd/solana-hsm.service` | HSM systemd unit (ayrı kullanıcı) |
| `.env.example` | Güvenli env şablonu (secret'sız) |
| `docs/VPS_RUNBOOK.md` | Operasyonel runbook |
| `docs/HSM_MTLS_OPS.md` | HSM/mTLS operasyon prosedürü |
