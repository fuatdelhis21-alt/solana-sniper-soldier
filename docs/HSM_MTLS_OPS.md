# HSM / mTLS Operasyon Güvenliği

Bu doküman, `tools/hsm_server` (remote HSM signer) ile bot arasındaki mTLS
operasyon güvenliğini açıklar. **Mainnet işlemi YASAKTIR.** Yalnızca
local/mock/devnet test certificate yaklaşımı kullanılır; gerçek production
certificate/key üretilmez.

---

## 1. HSM Process Health Check

HSM, `GET /pubkey` endpoint'i üzerinden mTLS ile health + pubkey döndürür.
Bot, live başlangıcında bu endpoint'i çağırır:

```bash
curl -s --cert /etc/solana-hft/hsm/client_all.pem \
     --cacert /etc/solana-hft/hsm/ca.pem \
     https://127.0.0.1:8443/pubkey
```

- Başarılı yanıt → HSM ayakta, pubkey biliniyor.
- Bağlantı hatası / timeout → HSM unavailable → bot live mode'a geçmez.

---

## 2. Pubkey Discovery / Verification

Bot, HSM'den dönen pubkey'i beklenen değerle karşılaştırır. **Wrong pubkey
durumunda live reddedilir** (fail-closed). Pubkey, HSM'in `HSM_KEY_B64`
environment değişkeninden türetilir; bot bu key'e asla erişmez.

---

## 3. mTLS Bağlantısının Doğrulanması

- Sunucu (`hsm_server`) client certificate ister; client cert yoksa bağlantı
  reddedilir (fail-closed, plain-HTTP fallback yok).
- Bot, `--hsm-ca` (sunucu CA) + `--hsm-client-identity` (client cert+key PEM)
  ile bağlanır.
- mTLS cert/key dosyaları source tree dışında, bot kullanıcısının okuyabildiği
  bir yolda tutulur.

---

## 4. Certificate Expiration Kontrolü

Operasyonel kontrol (runbook'a eklenir):

```bash
# Sunucu sertifikasının bitiş tarihi
openssl x509 -in /etc/solana-hft-hsm/certs/server.pem -noout -enddate

# Client sertifikasının bitiş tarihi
openssl x509 -in /etc/solana-hft/hsm/client_all.pem -noout -enddate
```

Bitişe 30 günden az kaldıysa rotation planlanmalıdır.

---

## 5. Sertifika Yenileme (Rotation) ve İptal (Revocation)

Özel CA (`generate_certs.sh`): CA 10 yıl, server/client 825 gün geçerli.
HSM sunucusu (warp 0.3 / rustls) **CRL veya OCSP kontrol etmez**: CA'nın
imzaladığı her geçerli client sertifikası kabul edilir. Bu yüzden tek bir
sertifikayı "iptal etmek" mümkün değildir — iptal = **yeni CA ile tüm PKI'yi
yeniden üretmek**. Bu sertifikalar imza anahtarı değildir: HSM'deki Solana
anahtarı (`HSM_KEY_B64`) rotation'dan etkilenmez ve hiçbir adımda
loglanmaz/kopyalanmaz.

**Planlı yenileme** (bitişe < 30 gün, bkz. §4) ve **iptal** (client
sertifikası veya `ca.key` sızdı / şüphe var) aynı prosedürü izler:

```bash
# 0. Açık live pozisyon varsa önce kapanmasını bekle ya da paper/dry-run'a geç.
#    Bot HSM'siz kalırsa yeni giriş yapmaz; açık pozisyonun exit'i HSM ister.
# 1. Yeni PKI (yeni CA dahil) — eski dizinin üzerine YAZMA:
/opt/solana-hft/generate_certs.sh /root/hft-pki-$(date +%Y%m%d)
# 2. Servisleri durdur
systemctl stop solana-hft solana-hft-sniper solana-hsm
# 3. Yeni dosyaları yerleştir (eski CA güven listesinden tamamen çıkar)
cp /root/hft-pki-YYYYMMDD/{server.pem,server.key,ca.pem} /etc/solana-hft/hsm-certs/
cp /root/hft-pki-YYYYMMDD/{ca.pem,client_all.pem}       /etc/solana-hft/client-certs/
chown -R solana-hsm: /etc/solana-hft/hsm-certs   && chmod 600 /etc/solana-hft/hsm-certs/*
chown -R solana:     /etc/solana-hft/client-certs && chmod 600 /etc/solana-hft/client-certs/*
# 4. Başlat ve doğrula
systemctl start solana-hsm
openssl x509 -in /etc/solana-hft/hsm-certs/server.pem -noout -enddate
curl -s --cacert /etc/solana-hft/client-certs/ca.pem \
     --cert /etc/solana-hft/client-certs/client_all.pem https://127.0.0.1:8443/pubkey
#    → pubkey SNIPER_EXPECTED_SIGNER ile aynı olmalı
# 5. Eski sertifikayla bağlanılamadığını doğrula (iptalin kanıtı):
curl -s --cacert /etc/solana-hft/client-certs/ca.pem \
     --cert /root/hft-pki-ESKI/client_all.pem https://127.0.0.1:8443/pubkey \
  && echo "HATA: eski client sertifikası hâlâ kabul ediliyor" || echo "eski sertifika reddedildi"
# 6. Botu başlat, /ready kontrol et
systemctl start solana-hft-sniper   # veya solana-hft
# 7. ca.key'i sunucudan kaldır (çevrimdışı sakla) ve eski PKI dizinini güvenle sil:
shred -u /root/hft-pki-ESKI/*.key && rm -rf /root/hft-pki-ESKI
```

Notlar:
- Bot HSM'e bağlanamazsa **yedek imzacıya düşmez** (yerel anahtar fallback'i
  yoktur); breaker açılır, yeni giriş durur.
- `ca.key` sunucuda kaldığı sürece onu ele geçiren yeni client sertifikası
  üretebilir; yenilemeden sonra çevrimdışı tutulmalıdır.
- HSM yalnızca 127.0.0.1:8443'te dinler; mTLS ikinci savunma hattıdır.

---

## 6. HSM Restart Sonrası Bot Davranışı

Bot, live modda HSM restart sonrası pubkey + health'i **yeniden doğrular**.
Doğrulama başarısızsa:
- Breaker Open'a gider (fail-closed).
- Yeni entry açılmaz.
- `/ready` 503 döner.

---

## 7. Wrong Pubkey Davranışı

Beklenen pubkey ile HSM'den dönen pubkey eşleşmezse:
- Live startup reddedilir.
- Audit loguna `hsm_pubkey_mismatch` yazılır.
- Bot private key'e erişemez; yalnızca mTLS signing request gönderir.

---

## 8. Signing Timeout Davranışı

HSM signing isteği timeout'a düşerse:
- İşlem imzalanmaz, gönderilmez.
- Breaker Open'a gider (fail-closed).
- `hft_hsm_errors` metriği artar.

---

## 9. Duplicate Signing Request Davranışı

Aynı işlem için tekrarlanan signing isteği, HSM tarafında idempotent değilse
çift imza riski doğurur. Bot, exit retry bütçesi (max 3) ile sınırlıdır;
EXIT_BLOCKED sonrası yeni istek gönderilmez. Bu, duplicate signing riskini
sınırlar.

---

## 10. HSM Unavailable Davranışı

HSM erişilemezse:
- Live modda yeni entry açılmaz.
- Breaker Open'a gider.
- `/ready` 503 döner.
- Operatör `systemctl start solana-hsm` ile HSM'i başlatır; bot yeniden
  doğrular.

---

## 11. Botun Private Key'e Erişemediğinin Doğrulanması

```bash
# HSM private key dosyası solana-hsm sahipli, 0600
sudo -u solana-bot cat /etc/solana-hft-hsm/hsm.env 2>&1 | grep -q "Permission denied" && echo "bot erişemez (OK)"
```

---

## 12. Signing Request Loglarının Secret İçermediğinin Doğrulanması

```bash
# HSM audit logunda secret arama
sudo -u solana-hsm grep -iE "private key|HSM_KEY|BEGIN|secret" /var/log/solana-hft-hsm/hsm_audit.log || echo "temiz"
```

Signing request logları yalnızca event + reason içerir; imza payload'ı veya
private key loglanmaz.
