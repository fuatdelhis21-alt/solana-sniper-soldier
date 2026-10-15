# Solana HFT Platform — Deployment Checklist

Ayrıntılı adımlar: [`README_DEPLOY.md`](README_DEPLOY.md). Kanıta dayalı durum:
[`docs/READINESS.md`](docs/READINESS.md). Mainnet onay listesi:
[`docs/mainnet_checklist.md`](docs/mainnet_checklist.md). Arıza prosedürleri:
[`docs/VPS_RUNBOOK.md`](docs/VPS_RUNBOOK.md) §9.

Durum: **paper için hazır, live için değil.** Live'a geçiş kararı operatöre aittir.

## 1. Sürüm (CI)

Her PR ve `main` push'unda `.github/workflows/ci-cd.yml` şunları çalıştırır;
hepsi yeşil olmadan sürüm kullanılmaz:

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo build --release` (workspace: bot, pool-sniper, HSM sunucusu)
- [ ] Deploy script'leri için `bash -n`
- [ ] HSM mTLS smoke test (`hsm-mtls-smoke-test.yml`)
- [ ] `main` üzerinde: paket (`scripts/build_and_publish.sh`) + artifact

## 2. Kurulum (VPS)

- [ ] `deploy/install_release.sh` ile kurulum; `REVISION` ve `SHA256SUMS` doğrulandı.
- [ ] mTLS sertifikaları üretildi ve yerleştirildi (`README_DEPLOY.md`);
      `ca.key` sunucudan kaldırıldı. Yenileme/iptal: `docs/HSM_MTLS_OPS.md` §5.
- [ ] `hsm.env` yalnızca root/`solana-hsm` okuyabilir (0600). HSM, `HSM_KEY_B64`
      olmadan **başlamaz** — üretilmiş/geçici anahtar ya da yedek imzacı yoktur.
- [ ] `.env` `.env.example`'dan; yeni ayarlar dahil (`SNIPER_EXIT_IMPACT_PCT`,
      `SNIPER_FAILED_TX_RESERVE`, `SNIPER_MAX_POSITION_LIQUIDITY_BPS`,
      `SNIPER_PAPER_LATENCY_BPS`, `SNIPER_SIGNAL_MAX_AGE_MS`).
- [ ] `SNIPER_LIVE_RISK_DATA=true` (dry-run'da da: aksi halde likidite
      doğrulanamaz ve her giriş `liquidity_unverified` ile reddedilir).
- [ ] Prometheus kuralları `deploy/prometheus/solana-hft-alerts.yml` yüklendi;
      `/health` ve `/ready` (bot ve pool-sniper `127.0.0.1:9091`) izleniyor.

## 3. Koddaki sabit risk sınırları (`RiskConfig::production_defaults`)

| Sınır | Değer | Davranış |
|---|---|---|
| İşlem başı | 0.05 SOL (strateji 0.01 SOL kullanır, `SNIPER_MAX_SPEND_SOL`) | Üstü reddedilir |
| Günlük işlem | 5 | Aşılırsa kill switch |
| Günlük zarar | 0.20 SOL | **Limite ulaşınca (`>=`)** yeni giriş yok + kill switch |
| Açık pozisyon / toplam maruziyet | 1 / 0.05 SOL | Üstü reddedilir |
| Kayma | ≤ 2 % (varsayılan istek 1 %) | Üstü reddedilir |
| Fiyat tazeliği | 5 s (`PRICE_STALENESS_MS`) | Eski = `stale_price`, gelecek zaman damgası = `clock_skew` |

Çıkışlar (stop-loss -5 %, take-profit +10 %, max hold, likidite düşüşü) bu
sınırlardan ve kill switch'ten **etkilenmez**.

## 4. Mod sırası

- [ ] **Paper** (varsayılan): ≥ 15–20 kapanmış işlem, `sniper_report.py` ile
      READINESS.md kabul listesi. Bu bir duman testidir, kârlılık kanıtı değildir.
- [ ] **Dry-run**: likit havuzda ve bir launch havuzunda giriş + çıkış simülasyonu OK
      (`exact entry quote`, `EXIT simulation … OK`).
- [ ] **Live** (yalnızca operatör kararıyla): `SNIPER_LIVE=true`,
      `SNIPER_CONFIRM_LIVE=true`, `SNIPER_EXPECTED_SIGNER`, HSM + mTLS; ≤ 0.1 SOL fon;
      `SolanaHftExitBlocked` alarmında dur ve runbook'u uygula.

## 5. Operatörün yapması gerekenler (kodla çözülemez)

- [ ] Ücretli mainnet RPC/WS → `SNIPER_RPC`, `SNIPER_WS`
- [ ] Gerçek imza anahtarı yalnızca `hsm.env` içinde (`HSM_KEY_B64`); pubkey →
      `SNIPER_EXPECTED_SIGNER`. Anahtar hiçbir zaman repoya, loga veya sohbete girmez.
- [ ] Cüzdan fonlama (işlem + ücretler + ATA rent ≈ 0.003 SOL + 0.006 SOL rezerv)
- [ ] (CI'da S3 yüklemesi istenirse) `S3_ENDPOINT`, `S3_BUCKET`, `AWS_*` secret'ları
- [ ] Geri dönüş: `deploy/rollback.sh`
