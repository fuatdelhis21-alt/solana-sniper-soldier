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

## 5. Certificate Rotation Prosedürü

Devnet/test için `tools/hsm_server/certs/generate_certs.ps1` kullanılır.
Production rotation için ayrı, operatör onaylı prosedür gerekir (bu görev
kapsamında gerçek production cert üretilmez).

Rotation adımları (devnet/test):
1. Yeni cert seti üret.
2. HSM servisini durdur, yeni cert'leri kopyala (0600, `solana-hsm` sahipli).
3. HSM'i başlat, `GET /pubkey` ile doğrula.
4. Bot'un mTLS client identity'sini güncelle.
5. Bot'u restart et, `/ready` ile live readiness'i doğrula.

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
