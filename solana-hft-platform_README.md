# Solana HFT Platform

Ultra-düşük gecikmeli (ultra-low-latency), production-grade bir Solana trading
platformunun **çekirdek temeli (foundation)**. Bu repo, ileride eklenecek büyük
modüllerin (Market Data, Signal Engine, Risk Engine, Security, Execution)
üzerine inşa edileceği sağlam, gözlemlenebilir ve tamamen test edilebilir bir
altyapı sağlar.

> **Faz Durumu:** Bu faz yalnızca **çekirdek altyapıyı** (core infrastructure)
> içerir. Trading mantığı sonraki fazlarda eklenecektir.

---

## Proje Felsefesi

- **Production-first, deterministik yürütme** — Floating-point non-determinizmi
  sıcak yoldan uzak tutulur; fiyat/skorlar sabit noktalı tam sayıdır.
- **Zero-trust veri doğrulama** — Tüm girdiler sınırda katı biçimde doğrulanır.
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
│   ├── default.toml           # Güvenli varsayılanlar (development)
│   └── production.example.toml# Production şablonu (kopyalayıp kullanın)
└── crates/
    ├── hft-core/              # Çekirdek tipler + hata yönetimi + eşzamanlılık
    ├── hft-config/            # TOML/ENV konfigürasyon + doğrulama + hot-reload
    ├── hft-telemetry/         # Yapısal loglama (tracing) + Prometheus metrikleri
    └── hft-integration/       # Uçtan uca entegrasyon testleri
```

### Crate'ler

| Crate            | Sorumluluk                                                        |
|------------------|-------------------------------------------------------------------|
| `hft-core`       | `Price`, `OrderBook`, `Trade`, `Signal`, `Position`, `RiskLimits`, hata tipleri, kilitsiz atomik yapılar |
| `hft-config`     | TOML + ENV yükleme, zero-trust doğrulama, `ArcSwap` ile lock-free hot-reload |
| `hft-telemetry`  | `tracing` tabanlı JSON/pretty loglama, Prometheus + sıcak yol inline metrikleri |
| `hft-integration`| Üç crate'in birlikte çalıştığını doğrulayan entegrasyon testleri  |

Her crate'in kendi `README.md`'si vardır.

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
# Tüm birim + entegrasyon testleri
cargo test

# Sadece entegrasyon testleri
cargo test -p hft-integration
```

### Benchmark

```bash
cargo bench -p hft-core
```

### Örnek Kullanım

```rust
use hft_config::load_from_file;
use hft_telemetry::logging::{init_logging, LogConfig};
use hft_telemetry::metrics::MetricsHub;

fn main() -> hft_core::HftResult<()> {
    // 1) Konfigürasyonu yükle (kilitsiz okuma için handle döner).
    let cfg = load_from_file("config/default.toml")?;

    // 2) Loglamayı başlat.
    let telemetry = &cfg.load().telemetry;
    init_logging(&LogConfig {
        filter: telemetry.log_level.clone(),
        format: hft_telemetry::logging::format_from_bool(telemetry.json_logs),
    })?;

    // 3) Metrikleri kur.
    let metrics = MetricsHub::new()?;
    metrics.inline.market_msgs.inc(); // sıcak yol: kilitsiz sayım

    tracing::info!("platform foundation hazır");
    Ok(())
}
```

---

## Konfigürasyon

- Varsayılanlar `config/default.toml` içindedir.
- **Gizli/hassas değerler dosyaya yazılmaz**; `HFT_` önekli ortam değişkenleri
  ile enjekte edilir. Örnek:
  ```bash
  export HFT_MARKET_DATA__GRPC_ENDPOINT="https://ozel-geyser:443"
  export HFT_GENERAL__ENVIRONMENT="production"
  ```
- **Hot-reload:** `ConfigWatcher` dosya değişikliklerini izler, doğrular ve
  atomik olarak uygular. Geçersiz değişiklik reddedilir, mevcut config korunur.

---

## Gözlemlenebilirlik (Observability)

- **Loglama:** Yapısal, JSON (production) veya pretty (geliştirme). Devre dışı
  log seviyeleri neredeyse sıfır maliyetlidir.
- **Metrikler:** Prometheus uyumlu `/metrics` çıktısı + sıcak yolda kilitsiz
  atomik sayaçlar. Gecikme histogramı mikrosaniye çözünürlüğünde.

---

## Kod Kalitesi

- Production-ready, **placeholder yok**.
- Kapsamlı **Türkçe** yorumlar.
- **Type safety first** — yanlış kullanım derleme zamanında engellenir.
- **Deterministik davranış** — integer aritmetiği, sabit boyutlu yapılar.
- Tüm public API'ler dokümante (`#![warn(missing_docs)]`).

---

## Yol Haritası (Sonraki Fazlar)

1. **Market Data Layer** — Yellowstone/Geyser gRPC, zero-copy parsing.
2. **Signal Engine** — deterministik matematiksel sinyaller.
3. **Risk Engine** — pozisyon boyutlandırma, maruziyet yönetimi.
4. **Security Layer** — rug-pull tespiti, scam filtreleme.
5. **Execution Layer** — Jito bundle (birincil), RPC (fallback).

---

## Lisans

MIT
