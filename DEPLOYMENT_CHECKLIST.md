# Solana HFT Platform — Deployment Checklist

Ayrıntılı adımlar: [`README_DEPLOY.md`](README_DEPLOY.md). Mainnet onay
listesi: [`docs/mainnet_checklist.md`](docs/mainnet_checklist.md).

## Kod tarafında giderilen canlıya geçiş engelleri

- [x] Swap talimatı yanlış Raydium CLMM program ID'sine gidiyordu
      (`CAMMCzo5YLJbYF7r5...` hiçbir mainnet havuzuna ait değil) → tek kaynak
      `account_resolver::RAYDIUM_CLMM_PROGRAM_ID`, adapter'a cluster'a göre set ediliyor;
      WebSocket `programSubscribe` da düzeltildi.
- [x] Live çıkış (exit) yolu yoktu: pozisyon açılıyor ama hiç satılmıyordu ve
      `max_open_positions=1` yüzünden ilk işlemden sonra bot duruyordu → stop-loss /
      take-profit ters swap, kalıcı `live_position.json`, gerçek bakiye farkıyla P&L.
- [x] Swap tx'i sendable değildi (çıkış ATA'sı yok, SOL wrap yok, priority fee yok)
      → idempotent ATA oluşturma, wSOL wrap/unwrap, compute budget.
- [x] HSM imzası doğrulanmıyordu (sunucu hata durumunda HTTP 200 + sıfır imza dönüyordu)
      → istemci Ed25519 doğrulaması, `--expected-signer` pin (live'da zorunlu), sunucu 400 +
      `HSM_REQUIRE_KEY=1`.
- [x] Prometheus `/metrics` + `/health` sunucusu hiç başlatılmıyordu → `--metrics-addr`.
- [x] Sürekli çalışma yoktu (30 iterasyon sonra çıkış) → `--iterations 0`, SIGTERM ile
      graceful stop; geçici circuit breaker artık servisi sonlandırmıyor.
- [x] Config bağlı değildi → her flag `SNIPER_*` env değişkeniyle, `.env` otomatik yüklenir.
- [x] systemd unit'i olmayan binary'yi (`solana-hft --config`) çalıştırıyordu; install
      script `set -u` ile `$2` yüzünden çöküyordu; paket tüm `target/` dizinini içeriyordu;
      Ansible `remote_src` hatalıydı → hepsi yeniden yazıldı, HSM için ayrı unit, rollback.
- [x] CI: deprecated `actions-rs/toolchain`, kırmızı `risk::tests::audit_log_written_on_kill_switch`.

- [x] Swap ücreti %0.05 sabitti → havuzun gerçek ücret kademesi AmmConfig'ten okunuyor.
- [x] Yalnızca mevcut tick'in dizisi veriliyordu → swap yönündeki ilk başlatılmış 3 tick
      array seçiliyor (boş dizi / sınır geçişi zincirde hata vermez).
- [x] Jito bahşişi hiç eklenmiyordu → tip hesapları Block Engine'den canlı okunuyor,
      bahşiş transferi imzadan önce işleme ekleniyor; bahşişsiz Jito live'da reddediliyor.
- [x] `getTokenLargestAccounts` her turda çağrılıyordu → 60 sn önbellek.

## Operatörün yapması gerekenler (kodla çözülemez)

- [ ] Ücretli mainnet RPC (Helius/Triton/QuickNode) → `SNIPER_RPC`, `SNIPER_WS`
- [ ] Sunucu + SSH erişimi, `inventory.ini`
- [ ] Gerçek cüzdan anahtarı → `hsm.env` (`HSM_KEY_B64`), pubkey → `SNIPER_EXPECTED_SIGNER`
- [ ] Cüzdanı fonlama (işlem başı 0.01 SOL + ücretler + ATA rent ≈ 0.003 SOL)
- [ ] Hedef havuz: `SNIPER_POOL_ID`, `SNIPER_OUTPUT_MINT` (mint/freeze authority'si
      kaldırılmış token; aksi halde risk kapısı reddeder)
- [ ] Paper → dry-run → live sırası; live için `SNIPER_LIVE=true` + `SNIPER_CONFIRM_LIVE=true`
- [ ] (CI'da S3 yüklemesi istenirse) `S3_ENDPOINT`, `S3_BUCKET`, `AWS_*` secret'ları
