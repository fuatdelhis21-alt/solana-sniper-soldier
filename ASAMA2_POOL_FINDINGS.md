# AŞAMA 2 — Uygun Mainnet Havuz Araştırması (2026-09-14)

## Özet

Gerçek fiyat hareketi olan bir mainnet Raydium CLMM havuzu arandı. **Hiçbir havuz
tüm kapılardan geçemedi.** Bunun nedeni havuz bulunamaması değil — **kodda iki
yapısal hata** olduğu ortaya çıktı.

## Yöntem

- Raydium API v3 (`/pools/info/list`, `/pools/info/mint`) ile mainnet CLMM havuzları
- Helius mainnet RPC ile on-chain doğrulama
- `parse_pool_state` offset'leri kodla birebir aynı (mint0=73, mint1=105,
  vault0=137, vault1=169, liquidity=237, sqrt_price=253)
- Kapılar koddan birebir kopyalandı:
  - `min_liquidity_lamports = 1_000_000_000_000` (1000 SOL)
  - `MAX_SINGLE_HOLDER_PCT = 30.0`, `MAX_TOP20_HOLDER_PCT = 70.0`
  - `fetch_mint_authority_risk` → `is_risky()` = mint VEYA freeze authority var

## Tarama sonuçları

### 1. tarama: 600 havuz (6 sayfa), wSOL içeren 50 mainnet CLMM
- 20 sn içinde fiyatı hareket eden: **19**
- Tüm kapılardan geçen: **0**

Reddetme dağılımı:
| Neden | Adet |
|---|---|
| mint/freeze authority var | 9 |
| likidite < 1000 SOL | 4 |
| supply = 0 | 4 |
| (diğer) | 2 |

### 2. tarama: holder kapısından geçen tokenlerin havuzları
Holder kapısı 10 büyük token üzerinde test edildi:

| TOKEN | top1 % | top20 % | Sonuç |
|---|---|---|---|
| RAY | 24.97 | 82.68 | REJECT |
| JUP | 24.78 | 72.67 | REJECT |
| BONK | 8.83 | 51.57 | PASS |
| PYTH | 12.72 | 60.26 | PASS |
| JTO | 21.26 | 60.70 | PASS |
| WIF | 13.72 | 56.08 | PASS |
| ORCA | 18.93 | 76.09 | REJECT |
| MNDE | 21.94 | 79.26 | REJECT |
| TNSR | 20.02 | 84.14 | REJECT |
| DRIFT | 24.49 | 67.73 | PASS |

Geçen 5 tokenin 42 CLMM havuzu tarandı → **0 PASS**.

---

## BULGU 1 (KRİTİK): Holder kapısı wSOL'un kendi arzını ölçüyor

`resolve_paper_market_data()` (main.rs:341) holder istatistiklerini **`input_mint`**
üzerinden çekiyor:

```rust
let stats = onchain_risk::fetch_holder_stats(rpc_client, &input_mint, &vaults)
```

`input_mint` wSOL olduğunda (ki tüm aday havuzlarda öyle), wSOL'un kendi arzı
ölçülüyor. wSOL native mint olduğu için:

```
getTokenSupply(So111...112) -> amount = "0"
getTokenLargestAccounts(So111...112) -> RPC hatası (result yok)
```

Sonuç: `fetch_holder_stats` → `Err("mint ... has zero supply")` →
`PaperDataError::Unavailable` → **her iterasyon `data_error`**.

Bu, mevcut paper havuzunun (`EzhAKYQNgL...`) 79k kararının **1.301'inin
`data_error`** olmasının ve yeni havuzların hiçbirinin çalışmamasının nedenidir.

**Doğrulama:** PYTH'in en likit CLMM havuzu (`9n3dSLrERZQp95dHXywft7xV8D8xnGFLaUHtEhQVaXaC`,
1.993 SOL likidite, authority yok) tam da bu nedenle reddedildi.

**Düzeltme önerisi:** Holder kapısı **riskli taraf** üzerinden ölçülmeli —
yani wSOL olmayan mint. `input_mint`/`output_mint`'ten hangisi wSOL değilse o
kullanılmalı. (wSOL zaten güvenilir kabul edilir; rug riski memecoin tarafındadır.)

---

## BULGU 2: `getTokenLargestAccounts` yapısal olarak 20 hesapla sınırlı

SPL RPC standardı gereği `getTokenLargestAccounts` **en fazla 20 hesap** döndürür.
Kod bunu "top-20 holder" olarak yorumluyor ve %70 eşiğiyle karşılaştırıyor.

Sonuç: dağıtılmış arzı olan tokenlerde bile top-20 toplamı kolayca %70'i aşıyor
(RAY %82.7, TNSR %84.1, MNDE %79.3). Bu, kapının **aşırı katı** olduğu ve
gerçekçi havuzların çoğunu elediği anlamına geliyor.

**Not:** Bu bir tasarım tercihi olabilir (fail-closed). Ancak pratikte
AŞAMA 2/3 için havuz bulmayı çok zorlaştırıyor. Operatör kararı gerekli.

---

## BULGU 3: Stablecoin havuzları yapısal olarak dışlanmış

USDC ve USDT'nin **mint authority'si var** (Circle/Tether — meşru):

```
USDC  mintAuthority=BJE5MMbqXjVwjAF7oxwPXnTXDyspzZyt4vwenNw5ruG  RISKY=True
USDT  mintAuthority=Q6XprfkF8RQQKoQVG33xT88H7wi8Uk1B1CC7YAs69Gi  RISKY=True
wSOL  mintAuthority=None  freezeAuthority=None  RISKY=False
```

`is_risky()` mint VEYA freeze authority varlığına baktığı için **tüm
wSOL/USDC ve wSOL/USDT havuzları her zaman reddedilir** — likiditeleri
ne olursa olsun. En likit, en düşük riskli havuz sınıfı yapısal olarak
kullanılamaz durumda.

---

## Fiyat hareketi doğrulaması (kapılar geçilmese de)

Gerçek fiyat hareketi olan havuzlar **mevcut** — sorun havuz kıtlığı değil:

| Havuz | Likidite | 60 sn'de benzersiz sqrt | Aralık |
|---|---|---|---|
| `2AXXcN6oN9bBT5owwmTH53C7QHUXvhLeu718Kqt8rvY2` (WSOL/RAY) | 7.632 SOL | 6/6 | 1.14 bps |
| `CBDLuFXmYFFvyCZhHmP9ugBNwHrxkumdUBjiGcx31HJo` (WSOL/CAPX) | 1.762 SOL | 5/6 | 3.95 bps |

Karşılaştırma: mevcut paper havuzu 30 saatte **1 benzersiz** sqrt_price gösterdi.

---

## Sonuç ve öneri

AŞAMA 2 için havuz bulunamamasının nedeni **havuz kıtlığı değil, koddaki iki
kapı hatasıdır**. Sırasıyla:

1. **BULGU 1 düzeltilmeli** (holder kapısı yanlış mint'i ölçüyor) — bu olmadan
   hiçbir wSOL havuzu çalışmaz.
2. **BULGU 2 için operatör kararı gerekli** (top-20 eşiği gerçekçi mi?).
3. **BULGU 3 için operatör kararı gerekli** (stablecoin authority istisnası?).

Bu üçü çözülmeden AŞAMA 3'e geçmek anlamsız — paper modu hiçbir havuzda
`enter` üretemez.

## Kullanılan scriptler

- `scripts/find_paper_pool.py` — geniş havuz taraması + kapı kontrolü
- `scripts/deep_check_pool.py` — tek havuz derin kontrol + fiyat hareketi
- `scripts/holder_gate.py` — holder kapısı detayı
- `scripts/holder_gate_scan.py` — holder kapısının tokenler üzerinde taraması
- `scripts/find_final_pool.py` — geçen tokenlerin havuz taraması
- `scripts/check_stablecoin_auth.py` — stablecoin authority doğrulaması
- `scripts/debug_pyth.py` — BULGU 1 kök neden kanıtı
