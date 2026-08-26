# Solana HFT Platform

Ultra-düşük gecikmeli (ultra-low-latency), production-grade bir Solana trading
platformu. Bu repo; market data, sinyal, risk ve yürütme katmanlarının yanı
sıra **Remote HSM imzalama altyapısını** (mTLS zorunlu, fail-closed) içerir.

> **Güvenlik duruşu:** Production/live işlem akışı **yalnızca** Remote HSM ile
> imzalanır. Local keyfile fallback'i yoktur; HSM bağlantısı, TLS handshake,
> client certificate veya imza doğrulaması başarısız olursa işlem akışı durur.

---

## Proje Felsefesi

- **Production-first, deterministik yürütme** — Floating-point non-determinizmi
  sıcak yoldan uzak tutulur; fiyat/skorlar sabit noktalı tam sayıdır.
- **Zero-trust veri doğrulama** — Tüm girdiler sınırda katı biçimde doğrulanır.
- **Fail-closed güvenlik** — İmza altyapısı kullanılamıyorsa işlem yapılmaz.
- **Ultra-düşük gecikme** — Hedef `<50ms`, ideal `<10ms` (tick-to-trade).
- **Modüler mimari** — Her sorumluluk ayrı bir crate'te; tam test edilebilir.
- **Sermaye koruması en yüksek öncelik** — Risk limitleri katı ve determinist.
- **Kaliteli trade > fazla trade.**

---

## Workspace Yapısı

```
solana-hft-platform/
├── Cargo.toml                 # Workspace kökü, merkezî bağımlılık yönetimi
├── config/                    # Örnek konfigürasyon dosyaları
├── crates/
│   ├── hft-core/              # Çekirdek tipler + hata yönetimi + eşzamanlılık
│   ├── hft-execution/         # Emir yürütme, Jito bundle, RPC, backend
│   └── hft-marketdata/        # Market data pipeline, dedup, ring buffer
├── solana-sniper/             # Ana trading botu (binary)
│   └── src/
│       ├── main.rs            # CLI + fail-closed imza akışı
│       ├── remote_hsm.rs      # Remote HSM mTLS istemcisi
│       ├── hw_signer.rs       # SignerAdapter trait + local/stub signer
│       ├── risk.rs            # Risk yönetimi + circuit breaker
│       ├── retry.rs           # Blockhash yönetimi + gönderim retry
│       └── bin/               # send_transfer, sign_test, ledger_sign_test
└── tools/
    └── hsm_server/            # Remote HSM imza sunucusu (warp + rustls mTLS)
        ├── main.rs            # POST /sign, GET /pubkey, audit log
        └── certs/             # mTLS PKI üretimi (generate_certs.ps1)
```

### Crate'ler

| Crate            | Sorumluluk                                                        |
|------------------|-------------------------------------------------------------------|
| `hft-core`       | `Price`, `OrderBook`, `Trade`, `Signal`, `Position`, `RiskLimits`, hata tipleri, kilitsiz atomik yapılar |
| `hft-execution`  | Emir yürütme, Jito bundle gönderimi, RPC backend, order tipleri   |
| `hft-marketdata` | Market data pipeline, deduplication, ring buffer, event/latency   |
| `solana-sniper`  | Ana bot: CLI, risk, karar, yürütme, Remote HSM imzalama           |
| `tools-hsm-server` | Remote HSM imza sunucusu (mTLS zorunlu, audit log)              |

---

## Güvenlik Mimarisi (Remote HSM + mTLS)

```
┌─────────────────────┐        mTLS (mutual TLS)        ┌──────────────────────┐
│  solana-sniper      │ ──────────────────────────────▶ │  hsm_server          │
│  (client / bot)     │  client_all.pem + ca.pem        │  warp + rustls       │
│                     │ ◀────────────────────────────── │  POST /sign          │
│                     │  signature (64B)                │  GET  /pubkey        │
└─────────────────────┘        pubkey (base58)          └──────────────────────┘
```

- **mTLS zorunlu:** Sunucu, `ca.pem` ile imzalanmış bir client certificate
  ister. Sertifikasız istemci reddedilir.
- **Fail-closed sunucu:** `server.pem` / `server.key` / `ca.pem` eksikse sunucu
  **plain HTTP'e düşmez**, başlamayı reddeder (exit 1).
- **Fail-closed istemci:** `--live` modu **yalnızca** `--hsm-endpoint` (+
  `--hsm-ca` + `--hsm-client-identity`) ile çalışır. Local keyfile asla
  yüklenmez; HSM hatası işlem akışını durdurur.
- **`--live` ve `--dry-run`** birlikte kullanılamaz (mutually exclusive).
- **Audit log:** Her imza isteği `logs/hsm_audit.log` içine yapısal JSON satırı
  olarak yazılır (`request_id`, `timestamp`, `status`, `tx_hash`). Private key
  veya wallet.json asla loglanmaz.

Detaylı operasyon rehberi: [`docs/HSM_SERVER_RUNBOOK.md`](docs/HSM_SERVER_RUNBOOK.md)

---

## Hızlı Başlangıç

### Ön Koşul: Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

### Derleme

```bash
# Geliştirme derlemesi
cargo build

# Production (optimize) derlemesi
cargo build --release
```

### Test

```bash
# Tüm workspace testleri
cargo test --workspace
```

---

## CLI Kullanımı (`solana-sniper`)

| Parametre | Açıklama |
|-----------|----------|
| `--rpc <URL>` | RPC endpoint (varsayılan `https://api.devnet.solana.com`) |
| `--ws <URL>` | WebSocket endpoint |
| `--wallet <path>` | Local keypair dosyası (yalnızca HSM'siz dry-run) |
| `--dry-run` | İşlem kur + imzala, gönderme (HSM'siz local keyfile kullanabilir) |
| `--live` | Gerçek işlem gönderimi — **HSM zorunlu** |
| `--iterations <N>` | İterasyon sayısı (varsayılan 30) |
| `--blockhash <b58>` | Dry-run için sabit blockhash (offline/deterministik test) |
| `--data-dir <path>` | Karar/log/metrik dizini (varsayılan `./data`) |
| `--hsm-endpoint <URL>` | Remote HSM endpoint (ör. `https://127.0.0.1:8443`) |
| `--hsm-ca <path>` | HSM sunucusunu doğrulamak için CA sertifikası (PEM) |
| `--hsm-client-identity <path>` | Sunucuya sunulan birleşik client cert + key (PEM) |

### Örnekler

```bash
# HSM'siz dry-run (local keyfile ile)
cargo run -p solana-sniper -- --dry-run --iterations 1

# Remote HSM ile dry-run (mTLS, deterministik — RPC'ye bağımlı değil)
cargo run -p solana-sniper -- --dry-run \
  --hsm-endpoint https://127.0.0.1:8443 \
  --hsm-ca tools/hsm_server/certs/ca.pem \
  --hsm-client-identity tools/hsm_server/certs/client_all.pem \
  --blockhash 11111111111111111111111111111111 \
  --iterations 1

# LIVE mod (yalnızca Remote HSM ile çalışır; local keyfile yüklenmez)
cargo run -p solana-sniper -- --live \
  --hsm-endpoint https://127.0.0.1:8443 \
  --hsm-ca tools/hsm_server/certs/ca.pem \
  --hsm-client-identity tools/hsm_server/certs/client_all.pem
```

---

## Remote HSM Sunucusu (`tools/hsm_server`)

```bash
# mTLS sertifikalarını üret
cd tools/hsm_server/certs
./generate_certs.ps1

# Sunucuyu başlat (mTLS zorunlu; certs yoksa başlamaz)
HSM_KEY_B64="<base64-64-byte-private-key>" \
  cargo run -p tools-hsm-server -- \
  --certs tools/hsm_server/certs \
  --log-file logs/hsm_audit.log
```

- **`POST /sign`** — base64 bincode `Transaction` alır, base64 64-byte imza döner.
- **`GET /pubkey`** — HSM'in sahip olduğu anahtarın base58 pubkey'ini döner
  (istemci local keyfile olmadan işlem kurabilir).
- **Audit log** — her istek için yapısal JSON satırı.

---

## Gözlemlenebilirlik (Observability)

- **Loglama:** Yapısal, JSON (production) veya pretty (geliştirme). Devre dışı
  log seviyeleri neredeyse sıfır maliyetlidir.
- **Metrikler:** `data/metrics.jsonl` içine periyodik metrik anlık görüntüleri.
- **Audit:** HSM imza istekleri `logs/hsm_audit.log` içinde tutulur.

---

## Kod Kalitesi

- Production-ready, **placeholder yok**.
- Kapsamlı **Türkçe** yorumlar.
- **Type safety first** — yanlış kullanım derleme zamanında engellenir.
- **Deterministik davranış** — integer aritmetiği, sabit boyutlu yapılar.
- Tüm public API'ler dokümante (`#![warn(missing_docs)]`).

---

## CI

`.github/workflows/hsm-mtls-smoke-test.yml` her push/PR'de:
1. mTLS PKI üretir, tüm binary'leri derler.
2. HSM sunucusunu arka planda başlatır ve `mTLS enabled` logunu bekler.
3. Pozitif (sertifikalı imzalama) ve negatif (sertifikasız red) testleri çalıştırır.
4. Audit log'da `"status":"signed"` kaydını doğrular.
5. Test sonunda sunucu process'ini temizler.

---

## Lisans

MIT
