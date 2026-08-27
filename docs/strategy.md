# Strateji Modülü — Giriş/Çıkış Kriterleri

Bu doküman, `solana-sniper/src/strategy.rs` içindeki `SimpleSnipeStrategy`'nin
giriş ve çıkış kriterlerini tanımlar. Tüm limitler **fail-closed** ve
**muhafazakâr** varsayılanlardır: sermaye koruması her zaman işlem sıklığı ve
kârlılıktan önce gelir.

## Giriş Kriterleri (Entry)

Bir token, aşağıdaki **tüm** filtreleri geçmeden işleme alınmaz. Herhangi bir
filtre başarısız olursa token **tamamen reddedilir** (kısmi giriş yok).

| Kriter | Varsayılan | Açıklama |
|--------|-----------|----------|
| `min_liquidity_lamports` | 1000 SOL | Havuz likiditesi bu eşiğin altındaysa reddet (düşük likidite = yüksek kayma/slippage riski). |
| `max_market_cap_lamports` | 1M SOL | Piyasa değeri bu tavanın üzerindeyse reddet (şişmiş / pump olmuş token). |
| `min_holders` | 50 | Holder sayısı bu minimumun altındaysa reddet (merkezileşme / rug riski). |
| `is_blocklisted` | false | Bilinen rug-pull / honeypot mint kara listesindeyse reddet. |

### Giriş sinyali üretildiğinde

- `position_size_lamports` = `max_trade_size_lamports` (varsayılan 0.1 SOL).
- `slippage_bps` = `max_slippage_bps` (varsayılan 100 bps = %1).
- `entry_sqrt_price` = giriş anındaki Q64.64 sqrt fiyatı (stop-loss/take-profit referansı).

## Çıkış Kriterleri (Exit)

Açık bir pozisyon için `should_exit(entry_sqrt_price, current_sqrt_price)` çağrılır.

| Karar | Koşul | Varsayılan |
|-------|-------|-----------|
| `StopLoss` | Fiyat, giriş fiyatının `stop_loss_bps` altına düşerse | %5 |
| `TakeProfit` | Fiyat, giriş fiyatının `take_profit_bps` üzerine çıkarsa | %10 |
| `Hold` | Fiyat bu iki bant arasındaysa | — |

Fiyat karşılaştırması Q64.64 sabit nokta aritmetiğiyle yapılır
(`ratio = current/entry`, `ratio²` fiyat değişim faktörüdür). Bu, kayan nokta
sapmasını önler ve deterministik sonuç verir.

## Güvenlik Notları

- Strateji modülü **saf** mantıktır: on-chain I/O yok, rastgelelik yok, deterministiktir.
- Tüm limitler `StrategyConfig` üzerinden yapılandırılabilir; varsayılanlar devnet için muhafazakârdır.
- Mainnet'e geçişte bu limitler operatör onayıyla ayrıca gözden geçirilmelidir (bkz. Step 7).
