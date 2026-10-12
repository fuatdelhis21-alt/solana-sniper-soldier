# Deployment Rehberi (bare-metal / VM, systemd)

Bu rehber; release paketini üretmeyi, sunucuya kurmayı, HSM + mTLS'i
yapılandırmayı ve botu **paper → dry-run → live** sırasıyla canlıya almayı
anlatır. Hiçbir adım otomatik olarak live moda geçmez: live için
`SNIPER_LIVE=true` **ve** `SNIPER_CONFIRM_LIVE=true` operatör tarafından
açıkça yazılmalıdır.

## 1. Release paketi

```bash
cargo test --workspace
./scripts/build_and_publish.sh        # -> solana-hft-release-<ts>.tar.gz
```

Paket içeriği: `bin/{solana-sniper,hsm_server,send_transfer}`, `systemd/*.service`,
`install_release.sh`, `rollback.sh`, `solana-hft.env.example`, `hsm.env.example`,
`generate_certs.sh`, `SHA256SUMS`, `REVISION`.
CI (`.github/workflows/ci-cd.yml`) `main`'e her push'ta aynı paketi üretir ve
Actions artifact'ı olarak yükler (S3 secret'ları varsa S3'e de).

## 2. Kurulum

```bash
# Sunucuda (root):
./install_release.sh solana-hft-release-<ts>.tar.gz [solana-hft.env]
# veya control node'dan:
ARTIFACT_PATH=./solana-hft-release-<ts>.tar.gz BOT_ENV_FILE=./prod.env \
  ansible-playbook -i inventory.ini ansible/deploy.yml
```

| Yol | İçerik |
|-----|--------|
| `/opt/solana-hft/bin/` | binary'ler (`bin.prev/` = önceki sürüm) |
| `/etc/solana-hft/solana-hft.env` | bot config (`SNIPER_*`), 0600 `solana` |
| `/etc/solana-hft/hsm.env` | `HSM_KEY_B64`, 0600 `solana-hsm` |
| `/etc/solana-hft/hsm-certs/` | `server.pem server.key ca.pem` |
| `/etc/solana-hft/client-certs/` | `ca.pem client_all.pem` |
| `/var/lib/solana-hft/` | `risk_state.json`, `live_position.json`, `logs/`, `audit/` |

## 3. HSM + mTLS

```bash
/opt/solana-hft/generate_certs.sh /root/hft-pki
cp /root/hft-pki/{server.pem,server.key,ca.pem} /etc/solana-hft/hsm-certs/
cp /root/hft-pki/{ca.pem,client_all.pem}       /etc/solana-hft/client-certs/
chown -R solana-hsm: /etc/solana-hft/hsm-certs
chown -R solana:     /etc/solana-hft/client-certs
# Cüzdan anahtarı (hsm.env.example içinde dönüştürme komutu var):
vi /etc/solana-hft/hsm.env           # HSM_KEY_B64=..., HSM_REQUIRE_KEY=1
systemctl start solana-hsm
```

`HSM_REQUIRE_KEY=1` ile sunucu geçerli anahtar yoksa **başlamaz** (ephemeral
anahtara düşmez). Bot, HSM'in bildirdiği pubkey'i `SNIPER_EXPECTED_SIGNER`
ile karşılaştırır ve her imzayı mesaj baytlarına karşı doğrular; uyuşmazlıkta
işlem yapılmaz.

## 4. Kademeli canlıya alma

`/etc/solana-hft/solana-hft.env` (şablon: `.env.example`):

1. **Paper** — `SNIPER_PAPER=true`, havuz/mint değerleri. Gerçek on-chain veri, tx yok.
2. **Dry-run** — `SNIPER_DRY_RUN=true`. Gerçek swap tx'i kurulur + HSM ile imzalanır, gönderilmez.
3. **Live** — `SNIPER_LIVE=true`, `SNIPER_CONFIRM_LIVE=true`, `SNIPER_LIVE_RISK_DATA=true`,
   `SNIPER_EXPECTED_SIGNER=<cüzdan pubkey>`.

```bash
systemctl restart solana-hft
journalctl -u solana-hft -f
curl -s 127.0.0.1:9090/health; curl -s 127.0.0.1:9090/metrics
```

Live akışı: giriş (wSOL → token) onaylanınca pozisyon `live_position.json`'a
yazılır; sonraki her iterasyonda gerçek havuz durumuyla değerlenir ve
stop-loss (-%5) / take-profit (+%10) sinyalinde ters swap ile **satılır**.
Gerçekleşen P&L bakiye farkından hesaplanır, zarar günlük limite işlenir.
Çıkışlar kill switch / circuit breaker tarafından engellenmez.

## 5. Durdurma, kill switch, rollback

- `systemctl stop solana-hft` → SIGTERM; mevcut iterasyon biter, açık pozisyon
  kaydı korunur ve bir sonraki başlatmada yönetilmeye devam eder.
- Günlük zarar / işlem limiti aşılırsa bot **exit 0** ile durur ve systemd
  yeniden başlatmaz (bilinçli). İnceleme sonrası `risk_state.json` temizlenip
  yeniden başlatılır.
- `/opt/solana-hft/rollback.sh` → `bin.prev`'e döner ve servisi yeniden başlatır
  (durum dosyalarına dokunmaz).
