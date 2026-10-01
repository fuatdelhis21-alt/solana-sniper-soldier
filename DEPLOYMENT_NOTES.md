# Deployment Notes — Strateji Güncellemeleri

## Sorun: 4 Gün İşlem Açılmamış

**Kök Neden:** Likidite eşiği çok yüksek (1000 SOL) — mainnet'te neredeyse hiç token geçmez.

## Çözüm: Strateji Parametrelerini Mainnet'e Uyarla

### Commit: `d390660` — "fix(strategy): reduce liquidity floor from 1000 SOL to 10 SOL for mainnet"

**Değişiklikler:**

| Parametre | Eski | Yeni | Neden |
|-----------|------|------|-------|
| `min_liquidity_lamports` | 1000 SOL | 10 SOL | Devnet-only; mainnet daha fragmented |
| `max_market_cap_lamports` | 1M SOL | 100M SOL | Yeni token launches'ları yakala, pump'lanmışları reddet |

**Diğer Parametreler (Değişmedi):**
- `max_trade_size_lamports`: 0.01 SOL (risk cap'ı altında)
- `max_slippage_bps`: 100 bps (1%)
- `stop_loss_bps`: 500 bps (5%)
- `take_profit_bps`: 1000 bps (10%)
- `max_daily_trades`: 20

**Holder Concentration Gates:**
- 30% single holder limit
- 70% top-20 holder limit
- `--live-risk-data` flag'i ile aktif (VPS'de zaten aktif)

## VPS Deployment

### Otomatik (SSH ile):
```bash
cd /opt/solana-sniper-soldier
git fetch origin
git checkout feat/perf-rpc-quote-cache
git pull origin feat/perf-rpc-quote-cache
cargo build --release
sudo systemctl restart solana-sniper-soldier.service
```

### Manuel (VPS'de):
```bash
ssh bot_service@31.97.125.104
cd /opt/solana-sniper-soldier
git pull origin feat/perf-rpc-quote-cache
cargo build --release 2>&1 | tail -50
sudo systemctl restart solana-sniper-soldier.service
sudo systemctl status solana-sniper-soldier.service
tail -50 /opt/solana-sniper-soldier/data/logs/hft.log.*
```

## Beklenen Sonuç

- **Daha fazla token aday** — 10 SOL likidite eşiği ile
- **İlk işlem açılması** — 1-2 saat içinde (pool'a bağlı)
- **Holder concentration gates** — Rug-pull'ları bloke eder

## Diğer Optimizasyonlar (Commit: `66dcbd0`)

- RPC blockhash refresh: 30s → 10s
- Send retry: 3 → 5 attempts, daha hızlı backoff
- Quote caching: Aynı parametreler için tekrarlanan quote'lar cache'den döndürülür

---

**Branch:** `feat/perf-rpc-quote-cache`
**Status:** Ready for deployment
