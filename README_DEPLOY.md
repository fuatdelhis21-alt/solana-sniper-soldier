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

Paket içeriği: `bin/{solana-sniper,pool-sniper,hsm_server,send_transfer}`, `systemd/*.service`,
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
| `/var/lib/solana-hft/` | `risk_state.json`, `live_position.json`, `pending_entry.json` (gönderilmiş ama sonucu kaydedilmemiş giriş — açılışta zincirden uzlaştırılır), `decisions/`, `logs/`, `audit/` |

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

Sunucu geçerli `HSM_KEY_B64` yoksa **başlamaz**; ephemeral anahtara düşme
yolu yoktur (yalnızca test için açık `--dev-ephemeral-key` bayrağı, systemd
kullanmaz). Her işlem imzalama politikasından geçer: yalnızca botun swap /
wSOL wrap / Jito tip şekli imzalanır, fonlar cüzdanın kendi hesaplarında
kalmak zorundadır, cüzdandan çıkan toplam ≤ `HSM_MAX_TRANSFER_LAMPORTS`
(varsayılan 0.06 SOL). Token transferi, yabancı hesaba swap çıktısı, başka
program → HTTP 400. Bot, HSM'in bildirdiği pubkey'i `SNIPER_EXPECTED_SIGNER`
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
# /health 503 = ana döngü takıldı (son iterasyon > 4 dk önce)
# Yeni havuz sniper'ı (solana-hft-sniper) kendi uç noktası:
curl -s 127.0.0.1:9091/health      # 503 = döngü takıldı veya WS log akışı 3 dk sessiz
curl -s 127.0.0.1:9091/metrics
```

Live akışı: giriş (wSOL → token) onaylanınca pozisyon `live_position.json`'a
yazılır; sonraki her iterasyonda gerçek havuz durumuyla değerlenir ve
stop-loss (-%5) / take-profit (+%10) sinyalinde ters swap ile **satılır**.
Gerçekleşen P&L bakiye farkından hesaplanır, zarar günlük limite işlenir.
Çıkışlar kill switch / circuit breaker tarafından engellenmez.

## 5. Yeni havuz sniper'ı (pool-sniper)

`solana-hft-sniper.service` sabit bir havuz yerine **yeni açılan** Raydium CLMM
havuzlarını işler:

1. `pool-sniper`, CLMM programının loglarını `logsSubscribe` ile dinler
   (`SNIPER_WS`), `CreatePool` görünce işlemi çözer (v0/ALT ve CPI dahil).
2. Yalnızca wSOL eşli havuzlar izleme listesine girer; liste
   `SNIPER_DISCOVERY_RECHECK_SECS` aralıkla `SNIPER_DISCOVERY_WATCH_SECS`
   boyunca kontrol edilir (havuz açıldığında likidite yoktur).
3. Ön filtre: klasik SPL mint, mint + freeze authority kapalı, SOL tarafı
   likidite ≥ `SNIPER_MIN_LIQUIDITY_SOL`.
4. Geçen havuz için **tek seferde bir** `solana-sniper --single-trade` süreci
   başlatılır (wSOL → token). Bu süreç tüm normal kapıları tekrar uygular
   (holder yoğunluğu, blocklist, risk yöneticisi, slippage, bakiye) ve çıkışı
   kendisi yönetir; `SNIPER_DISCOVERY_ENTRY_WINDOW_SECS` içinde giriş sinyali
   yoksa kapanır.
5. Tüm süreçler aynı `SNIPER_DATA_DIR`'i kullanır: günlük işlem/zarar
   limitleri ve tek açık pozisyon **tüm havuzlar için ortaktır**. Açık bir
   live pozisyon varsa yeni havuza geçilmez, önce o yönetilir.

```bash
systemctl stop solana-hft              # ikisi aynı anda çalışmaz (Conflicts=)
systemctl start solana-hft-sniper      # varsayılan: SNIPER_DISCOVERY_MODE=paper
journalctl -u solana-hft-sniper -f
```

Tüm havuzların toplu sonucu (her havuz ayrı süreçte çalıştığı için):

```bash
python3 /opt/solana-hft/sniper_report.py /var/lib/solana-hft --hours 24
```

Açık pozisyon için zorunlu çıkışlar (live ve paper aynı): stop-loss (-%5),
take-profit (+%10), `SNIPER_DISCOVERY_MAX_HOLD_SECS` süresi doldu, ya da SOL
tarafı likidite girişteki seviyenin **yarısına** düştü (likidite çekiliyor /
rug).

Kademeler aynıdır: `paper` → `dry-run` → `live`
(`SNIPER_DISCOVERY_MODE=live` + `SNIPER_DISCOVERY_CONFIRM_LIVE=true` + HSM).
Kalıcı geçiş için: `systemctl disable solana-hft; systemctl enable solana-hft-sniper`.

Sınırlar: bu, blok-0 hız yarışı değildir (onaylı log + RPC kontrolleri,
saniyeler mertebesi). Yeni tokenların büyük kısmı authority / holder / likidite
kapılarından geçemez; bu bilinçli. `logsSubscribe` CLMM'deki her işlemi
yayınlar — ücretli RPC planı önerilir.

## 6. Durdurma, kill switch, rollback

- `systemctl stop solana-hft` → SIGTERM; mevcut iterasyon biter, açık pozisyon
  kaydı korunur ve bir sonraki başlatmada yönetilmeye devam eder.
- Günlük zarar / işlem limiti aşılırsa bot **exit 0** ile durur ve systemd
  yeniden başlatmaz (bilinçli). İnceleme sonrası `risk_state.json` temizlenip
  yeniden başlatılır.
- `/opt/solana-hft/rollback.sh` → `bin.prev`'e döner ve servisi yeniden başlatır
  (durum dosyalarına dokunmaz).
