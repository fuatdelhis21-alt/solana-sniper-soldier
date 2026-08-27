# Mainnet Öncesi Kontrol Listesi

Bu doküman, botun **mainnet'e geçişi için gereken minimum manuel onay adımlarını**
tanımlar. **Uygulama adımı değildir** — yalnızca hazırlık ve doğrulama.

> **KRİTİK:** Mainnet'e geçiş, gerçek fon kullanımı veya sürekli/otonom trading
> loop'u başlatma **yalnızca operatörün açık manuel onayıyla** yapılmalıdır.
> Bu doküman onay için bir kontrol listesidir, otomatik geçişi tetiklemez.

---

## 1. Gerçek HSM Anahtarı

- [ ] `HSM_KEY_B64` ortam değişkeni **gerçek 64-byte mainnet private key** ile set edildi.
- [ ] Anahtarın public key'i, fonlanacak mainnet cüzdanıyla eşleşiyor.
- [ ] Anahtar **yalnızca HSM sunucusunda** tutuluyor; hiçbir yerde düz metin/loglanmıyor.
- [ ] Ephemeral/devnet anahtarı **kullanılmıyor** (mainnet için yetersiz ve güvensiz).

## 2. Gerçek Sertifikalar (mTLS)

- [ ] HSM sunucusu için **gerçek CA sertifikası** oluşturuldu ve güvenilir.
- [ ] HSM sunucusu **gerçek sunucu sertifikası** ile çalışıyor (self-signed devnet cert değil).
- [ ] Bot için **gerçek client sertifikası + private key** oluşturuldu.
- [ ] Sertifika private key'leri hiçbir yerde düz metin/loglanmıyor.
- [ ] mTLS fail-closed: sertifika yoksa HSM isteği reddediliyor.

## 3. Gerçek RPC Endpoint

- [ ] Mainnet RPC endpoint'i doğrulandı (örn. `https://api.mainnet-beta.solana.com`).
- [ ] RPC endpoint'i yüksek kullanılabilirlikli (HA) ve rate-limit yeterli.
- [ ] Jito Block Engine mainnet endpoint'i doğrulandı (`https://mainnet.block-engine.jito.wtf`).
- [ ] RPC fallback davranışı test edildi (Jito başarısız olursa RPC'ye düşer).

## 4. Gerçek Cüzdan Bakiyesi

- [ ] Mainnet cüzdanında işlem ücretlerini karşılayacak SOL mevcut.
- [ ] Bakiye doğrulama scripti çalıştırıldı ve beklenen değer döndü.
- [ ] Pozisyon büyüklüğü, cüzdan bakiyesinin güvenli bir yüzdesi (örn. ≤ %1).

## 5. Risk Limitleri (Mainnet İçin Gözden Geçirildi)

- [ ] `max_trade_size_lamports` mainnet için muhafazakâr değere ayarlandı.
- [ ] `max_daily_trades` mainnet için sınırlandı.
- [ ] `daily_loss_limit_lamports` mainnet için belirlendi.
- [ ] Kill switch tetikleme prosedürü operatör tarafından biliniyor.
- [ ] Strateji limitleri (`docs/strategy.md`) mainnet için gözden geçirildi.

## 6. Gözlemlenebilirlik

- [ ] Prometheus metrikleri canlı ve izleniyor.
- [ ] Grafana dashboard'u yüklendi ve paneller dolu.
- [ ] `/health` endpoint'i izleniyor (uptime).
- [ ] HSM audit log'u ve risk audit log'u izleniyor.

## 7. Test Tamamlandı

- [ ] Devnet canary testi (Step 6) başarılı: 5/5 on-chain, `meta.err` null.
- [ ] Paper-trading modu (Step 5) çalıştı, on-chain işlem göndermedi.
- [ ] Tüm birim testleri geçti (`cargo test`).

---

## Minimum Manuel Onay Adımları

Mainnet'e geçiş için operatörün **açıkça** onaylaması gereken adımlar:

1. **Fonlama onayı:** Mainnet cüzdanına gerçek SOL yatırılacak mı? (Miktar belirtilmeli)
2. **İlk işlem onayı:** İlk gerçek işlem için açık onay (küçük, test amaçlı).
3. **Loop onayı:** Sürekli/otonom trading loop'u başlatma onayı.
4. **Risk limiti onayı:** Mainnet risk limitleri (pozisyon, günlük işlem, zarar limiti) onayı.

> Bu adımların hiçbiri otomatik tetiklenmez. Bot, mainnet moduna geçmeden önce
> operatörün açık onayını bekler.
