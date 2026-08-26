# 🛡️ HSM Server Operations Runbook

**Platform:** Solana HFT — Remote HSM Signer
**Server:** `hsm_server` (package `tools-hsm-server`)
**mTLS Endpoint:** `https://127.0.0.1:8443`
**Certs:** `tools/hsm_server/certs/`
**Audit Log:** `logs/hsm_audit.log`

---

## 1. Mimariye Genel Bakış (Architecture Overview)

```
┌─────────────────────┐        mTLS (mutual TLS)        ┌──────────────────────┐
│  solana-sniper      │ ──────────────────────────────▶ │  hsm_server          │
│  (client / bot)     │  client_all.pem + ca.pem        │  warp + rustls       │
│                     │ ◀────────────────────────────── │  POST /sign          │
│                     │  signature (64B)                │  GET  /pubkey        │
└─────────────────────┘        pubkey (base58)          └──────────────────────┘
```

- **Client** (`solana-sniper/src/remote_hsm.rs`): rustls TLS, presents `client_all.pem` identity, trusts `ca.pem`.
- **Server** (`tools/hsm_server/main.rs`): requires client cert signed by `ca.pem` (`client_auth_required_path`). **Fail-closed**: if `server.pem`/`server.key`/`ca.pem` are missing the server refuses to start (no plain-HTTP fallback).
- **Endpoints**: `POST /sign` (returns base64 64-byte signature) and `GET /pubkey` (returns the signer's base58 pubkey so the client never needs a local keyfile).
- **Signing key**: loaded from `HSM_KEY_B64` env var (base64 64-byte private key). If missing/invalid → ephemeral keypair (dev/test only).

---

## 2. Acil Durum Müdahalesi (Emergency Response)

### 2.1 Sunucuyu Acil Durdurma
```powershell
Stop-Process -Name hsm_server -Force
```

Bir istemci sertifikası sızdırıldığında, tüm imzalama HIZLA durdurulmalıdır:

```powershell
# 1. Sunucuyu durdur (tüm istekleri kes)
Stop-Process -Name hsm_server -Force

# 2. Doğrula (hiçbir işlem kalmadı)
Get-Process -Name hsm_server -ErrorAction SilentlyContinue

# 3. Logları koru (adli inceleme için)
Copy-Item logs\hsm_audit.log "logs\hsm_audit_$(Get-Date -Format yyyyMMdd_HHmmss).log"
```

### 2.2 Bir İstemci Sertifikasını İptal Etme (Revocation)

Eğer bir `client_all.pem` sızdırılırsa:

1. **CRL (İptal Listesi) kullanın** (tercih edilen):
   - `tools/hsm_server/certs/` altında bir CRL oluşturun.
   - Sunucu, `client_auth_required_path` ile CA kökünü doğrular; CRL'yi sunucu destekliyorsa yapılandırın.
   - Revoked sertifikanın seri numarasını `openssl ca -revoke` ile işaretleyin.

2. **Basit / hızlı çözüm (tavsiye edilen):**
   - Mevcut `ca.pem`'i **yenileyin** (yeni kök CA + yeni anahtar).
   - Tüm güvenilir istemcilere **yeni sertifika dağıtın**.
   - Sunucuyu **yeni CA ile yeniden başlatın**.

   ```powershell
   # CA'yı yenileyip tüm sertifikaları yeniden üret
   cd tools\hsm_server\certs
   .\generate_certs.ps1
   ```

   > ⚠️ CA yenilendiğinde **tüm** mevcut client + server sertifikaları geçersiz olur. İstemcileri yeni `client_all.pem` ile güncellemeden yeniden başlatmayın.

### 2.3 İmzalama Anahtarının Sızdırılması (Signing Key Compromise)

`HSM_KEY_B64` değeri sızdırıldıysa:
1. Sunucuyu hemen durdurun (`Stop-Process -Name hsm_server -Force`).
2. `HSM_KEY_B64` değerini **geri alınamaz şekilde değiştirin** (yeni anahtar üretin).
3. Yeni anahtarla ilişkili Solana adresini doğrulayın ve fonları aktarın.
4. Sunucuyu yeni `HSM_KEY_B64` ile yeniden başlatın.

---

## 3. Sertifika Yönetimi (Certificate Management)

### 3.1 Tam PKI Yeniden Üretme
```powershell
cd tools\hsm_server\certs
.\generate_certs.ps1
```
Üretilen dosyalar:
| Dosya | Açıklama | Gizlilik |
|-------|----------|----------|
| `ca.key` | Kök CA özel anahtarı | 🔒 Sır — asla dağıtma |
| `ca.pem` | Kök CA sertifikası | ✓ İstemcilere dağıt |
| `server.key` | Sunucu özel anahtarı | 🔒 Sır |
| `server.pem` | Sunucu sertifikası (CA imzalı) | ✓ Sunucuda |
| `client.key` | İstemci özel anahtarı | 🔒 Sır |
| `client.pem` | İstemci sertifikası | ✓ İstemciye dağıt |
| `client_all.pem` | `client.pem` + `client.key` (birleşik) | 🔒 Sır — bot içinde |

### 3.2 Sertifika Dönüşümü (Rotation) — Planlı
```powershell
# 1. Yeni sertifikaları üret
cd tools\hsm_server\certs
.\generate_certs.ps1

# 2. Sunucuyu durdur
Stop-Process -Name hsm_server -Force

# 3. Yeni client_all.pem'i bot makinesine / güvenli depoya kopyala

# 4. Sunucuyu yeniden başlat (yeni certs ile)
.\start_server.bat

# 5. İmzalamanın çalıştığını doğrula (CI smoke test veya manuel)
```

### 3.3 Sertifikaları Doğrulama
```powershell
# Sunucu sertifikası
openssl x509 -in server.pem -noout -text

# CA doğrulaması
openssl verify -CAfile ca.pem server.pem
openssl verify -CAfile ca.pem client.pem

# client_all.pem içeriği (cert + key aynı dosyada olmalı)
Select-String -Path client_all.pem -Pattern "BEGIN (CERTIFICATE|PRIVATE KEY)"
```

---

## 4. Başlatma ve Durdurma (Startup & Shutdown)

### 4.1 Sunucuyu Başlatma
```powershell
# Geliştirme ortamı (ephemeral key veya HSM_KEY_B64 ile)
cd C:\Users\Lenovo\Downloads\solana-hft-platform
.\tools\hsm_server\start_server.bat

# VEYA doğrudan:
$env:HSM_KEY_B64 = "<base64-64byte-private-key>"
cargo run -p tools-hsm-server -- --certs tools/hsm_server/certs --log-file logs/hsm_audit.log
```

Başarılı başlatma çıktısı:
```
[hsm_server] mTLS enabled: serving HTTPS on 127.0.0.1:8443 (client cert required)
[hsm_server] certs: tools\hsm_server\certs
```

> ⚠️ **Fail-closed:** Eğer `server.pem`, `server.key` veya `ca.pem` eksikse sunucu **başlamayı reddeder** (`FATAL: mTLS certs not found ... Refusing to start in plain HTTP mode`). Sunucu asla plain HTTP olarak çalışmaz. Eksik sertifikaları `generate_certs.ps1` ile üretin.

### 4.2 Sunucuyu Durdurma
```powershell
Stop-Process -Name hsm_server -Force
```

### 4.3 Durum Kontrolü
```powershell
# İşlem çalışıyor mu?
Get-Process -Name hsm_server

# Port 8443 dinleniyor mu?
netstat -ano | findstr :8443

# mTLS aktif mi? (curl hatasız başarısız olmalı — sertifikasız)
curl -k -s --max-time 5 https://127.0.0.1:8443/sign --data "{}"
# Beklenen: bağlantı reddedilir (exit code 52/56/35 vb.) → mTLS doğru yapılandırılmış
```

---

## 5. Anahtar Yönetimi (Key Management)

### 5.1 `HSM_KEY_B64` Nasıl Çalışır
- Sunucu, imzalama anahtarını `HSM_KEY_B64` ortam değişkeninden (base64 kodlu 64-byte özel anahtar) yükler.
- **Boş / geçersizse**: Sunucu başlangıçta **ephemeral** (geçici) bir anahtar üretir ve pubkey'i loglar.
  ```
  [hsm_server] ephemeral signer pubkey: <PUBKEY>
  ```
- Ephemeral anahtar sunucu kapatıldığında **kaybolur** — bu yalnızca test/demo içindir.

### 5.2 Gerçek Anahtar Kullanma
```powershell
# 64-byte key'i base64'e çevir (örnek)
$key64 = [Convert]::ToBase64String([byte[]]@(0..63)) # veya gerçek key dosyasından
$env:HSM_KEY_B64 = $key64
.\tools\hsm_server\start_server.bat
```

### 5.3 Güvenlik Notları
- `HSM_KEY_B64`'i asla kaynak koduna, `start_server.bat`'a veya git'e yazmayın.
- Üretimde bunu bir secrets manager / environment vault'tan verin.
- Mevcut `start_server.bat` içinde `set HSM_KEY_B64=ZHVtbXk=` (`"dummy"`) placeholder olarak durur — **üretimde değiştirilmelidir**.

---

## 6. İzleme ve Denetim (Monitoring & Audit)

### 6.1 Denetim Logu
Her imzalama isteği `logs/hsm_audit.log` dosyasına JSON satırı olarak yazılır:
```json
{"timestamp":1786099991664,"request_id":"1786099991664-0","tx_hash":"...","status":"signed","ts_ms":1786099991664,"signature_b64":"..."}
```
Başarısız isteklerde güvenli bir `error` alanı eklenir (hassas veri içermez):
```json
{"timestamp":1786099991664,"request_id":"1786099991664-1","tx_hash":"<fallback>","status":"failed","ts_ms":1786099991664,"signature_b64":"<fallback>","error":"invalid base64 in tx field"}
```
- `timestamp`: Unix zaman damgası (ms) — sorgulama için ana alan
- `request_id`: `unix-ms` + işlem sıra numarası (`<ms>-<n>`)
- `tx_hash`: üretilen 64-byte imza (base64) — `signature_b64` ile aynı değer
- `status`: `"signed"` (başarılı) veya `"failed"` (geçersiz tx / fallback imza)
- `error`: yalnızca başarısız isteklerde; güvenli, hassas olmayan neden (`invalid base64 ...` / `invalid bincode transaction`). **Asla private key veya wallet.json içermez.**
- `ts_ms` / `signature_b64`: geriye dönük uyumluluk için korunan eski alanlar

### 6.2 Logları İnceleme
```powershell
# Son 20 imzalama isteği (yapılandırılmış tablo olarak)
Get-Content logs\hsm_audit.log -Tail 20 | ConvertFrom-Json | Select-Object timestamp, request_id, tx_hash, status

# Bugünkü istek sayısı
(Get-Content logs\hsm_audit.log | Measure-Object).Count

# Sadece başarısız imzalamalar
Get-Content logs\hsm_audit.log | ConvertFrom-Json | Where-Object { $_.status -eq "failed" }

# Belirli bir zaman aralığını filtrele
Get-Content logs\hsm_audit.log | ConvertFrom-Json | Where-Object { $_.timestamp -gt <start_ms> -and $_.timestamp -lt <end_ms> }
```

### 6.3 CI Otomasyonu
`.github/workflows/hsm-mtls-smoke-test.yml` her push/PR'de:
1. Tüm ikili dosyaları derler.
2. mTLS PKI üretir (`generate_certs.ps1`, pwsh + openssl — Linux runner'da çalışır).
3. Sunucuyu arka planda başlatır ve `mTLS enabled` logunu bekler.
4. Pozitif (sertifikalı imzalama, `--blockhash` ile deterministik) ve negatif (sertifikasız red) testleri çalıştırır.
5. Denetim logunda `request_id` + `tx_hash` + `status` + `timestamp` + en az bir `"status":"signed"` varlığını doğrular.
6. Test sonunda sunucu process'ini temizler (`pkill -f hsm_server`).

---

## 7. Sorun Giderme (Troubleshooting)

### 7.1 "FATAL: mTLS certs not found" (Fail-Closed)
**Belirti:** Başlatma logunda `[hsm_server] FATAL: mTLS certs not found at ... Refusing to start in plain HTTP mode (fail-closed).` ve sunucu çıkar (exit 1).
**Neden:** `server.pem` / `server.key` / `ca.pem` eksik. Sunucu **bilinçli olarak** plain HTTP modunda başlamayı reddeder (mTLS zorunlu).
**Çözüm:**
```powershell
cd tools\hsm_server\certs
.\generate_certs.ps1
```
Sunucuyu yeniden başlatın.

### 7.2 İstemci "remote hsm request failed"
**Belirti:** `Error: "remote hsm request failed: error sending request ..."`
**Nedenler / Çözümler:**
| Neden | Çözüm |
|-------|-------|
| Sunucu çalışmıyor | Sunucuyu başlatın (`start_server.bat`) |
| Port yanlış | `--hsm-endpoint https://127.0.0.1:8443` doğrulayın |
| `client_all.pem` eksik/bozuk | `generate_certs.ps1` çalıştırın, `client_all.pem` içeriğini doğrulayın |
| `ca.pem` sunucu CA ile uyuşmuyor | Aynı CA'nın `ca.pem`'ini kullanın |
| mTLS kapalı (sunucu HTTP) | Sunucu certs'lerini düzeltin |

### 7.3 İstemci Doğrulama Hatası (cert not trusted)
**Belirti:** TLS doğrulama hatası / sertifika zinciri güvenilmez.
**Çözüm:** `--hsm-ca` ile doğru `ca.pem` yolunu verin. `client_all.pem`'in doğru CA tarafından imzalandığını doğrulayın.

### 7.4 Fail-Closed Davranışı
Bot (`solana-sniper`) **LIVE modda** HSM sunucusuna **ulaşamazsa**:
- `--live` modu **yalnızca** `--hsm-endpoint` (+ `--hsm-ca` + `--hsm-client-identity`) ile çalışır; aksi halde başlamadan hata verir.
- HSM bağlantısı, TLS handshake, client certificate, imza cevabı veya imza doğrulaması başarısız olursa hata propagates → `Error: "remote hsm request failed: ..."` → **sıfır olmayan çıkış kodu** → bot durur.
- **LOCAL keyfile fallback YOK** — LIVE modda local keypair asla yüklenmez; bot sessizce trade'a devam etmez.
- `--live` ve `--dry-run` **birlikte kullanılamaz** (mutually exclusive).
```
$exitCode = $LASTEXITCODE  # 0 değilse → fail-closed doğru
```

---

## 8. Kurtarma Prosedürleri (Recovery)

### 8.1 Sunucu Çökmesi Sonrası
```powershell
# 1. Tüm örnekleri durdur
Stop-Process -Name hsm_server -Force -ErrorAction SilentlyContinue

# 2. Sunucuyu yeniden başlat
.\tools\hsm_server\start_server.bat

# 3. mTLS aktif mi doğrula (loglarda "mTLS enabled")

# 4. İmzalama testi yap
# CI smoke test veya manuel solana-sniper --dry-run
```

### 8.2 Sertifika/CDN Kaybı Sonrası
```powershell
# Tüm PKI'yı yeniden üret
cd tools\hsm_server\certs
.\generate_certs.ps1

# Yeni client_all.pem'i güvenli şekilde bot makinesine dağıt
# Sunucuyu yeniden başlat
```

### 8.3 Veri/Dosya Kaybı Kontrolü
- `logs/hsm_audit.log` — denetim geçmişi (silinirse yeni isteklerle yeniden oluşur)
- `tools/hsm_server/certs/` — PKI (kaybolursa yeniden üretilir)
- Yedek anahtar (`HSM_KEY_B64`) — kaybolursa ve ephemeral kullanılıyorsa imzalanan adres değişir.

---

## 9. Güvenlik Kontrol Listesi (Security Checklist)

- [ ] `ca.key` güvenli ortamda (yalnızca imzalama için erişilebilir)
- [ ] `server.key` sunucuda korunuyor
- [ ] `client_all.pem` bot makinesinde korunuyor (chmod / ACL)
- [ ] `HSM_KEY_B64` üretimde proper secrets manager'dan geliyor
- [ ] Sunucu logu `mTLS enabled` gösteriyor (HTTP değil) — sunucu certs yoksa başlamayı reddeder (fail-closed)
- [ ] `start_server.bat` içindeki `ZHVtbXk=` placeholder'ı üretimde kaldırıldı
- [ ] LIVE mod yalnızca `--hsm-endpoint` + mTLS certs ile çalışıyor; local keyfile fallback yok
- [ ] `--live` ve `--dry-run` birlikte kullanılmıyor (mutually exclusive)
- [ ] Denetim logu düzenli tutuluyor / yedekleniyor; private key / wallet.json içermiyor
- [ ] CI smoke testi her push'ta mTLS'i doğruluyor (positive + negative + audit)
- [ ] Acil durum kontakları / sorumlular tanımlı

---

## 10. Sık Kullanılan Komutlar (Quick Reference)

```powershell
# Başlat
.\tools\hsm_server\start_server.bat

# Durdur
Stop-Process -Name hsm_server -Force

# Durum / port
netstat -ano | findstr :8443

# Sertifika üret
cd tools\hsm_server\certs; .\generate_certs.ps1

# Denetim logu
Get-Content logs\hsm_audit.log -Tail 20

# İstemci bağlantı testi (dry-run, deterministik — RPC'ye bağımlı değil)
target\x86_64-pc-windows-msvc\debug\solana-sniper.exe --dry-run --iterations 1 `
  --hsm-endpoint https://127.0.0.1:8443 `
  --hsm-ca tools\hsm_server\certs\ca.pem `
  --hsm-client-identity tools\hsm_server\certs\client_all.pem `
  --blockhash 11111111111111111111111111111111

# HSM pubkey sorgula (mTLS ile)
curl -k --cert tools\hsm_server\certs\client_all.pem --cacert tools\hsm_server\certs\ca.pem `
  https://127.0.0.1:8443/pubkey
