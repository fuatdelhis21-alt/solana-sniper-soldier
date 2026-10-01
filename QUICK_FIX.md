# Hızlı Çözüm — 4 Gün İşlem Açılmamış

## Sorun
Bot canlı modda çalışıyor ama hiç işlem açmamış. Neden: **Likidite eşiği 1000 SOL** (devnet-only).

## Çözüm (3 adım)

### 1. VPS'ye SSH ile bağlan
```bash
ssh bot_service@31.97.125.104
```

### 2. Kodu güncelle ve build et
```bash
cd /opt/solana-sniper-soldier
git fetch origin
git checkout feat/perf-rpc-quote-cache
git pull origin feat/perf-rpc-quote-cache
cargo build --release
```

### 3. Servisi yeniden başlat
```bash
sudo systemctl restart solana-sniper-soldier.service
sudo systemctl status solana-sniper-soldier.service
```

## Ne Değişti?
- Likidite eşiği: 1000 SOL → **10 SOL** ✓
- Market cap ceiling: 1M SOL → **100M SOL** ✓
- RPC retry: Daha hızlı ✓
- Quote caching: Redundant hesaplamalar elimine ✓

## Beklenen Sonuç
- **1-2 saat içinde ilk işlem** (pool'a bağlı)
- Daha fazla token aday
- Holder concentration gates aktif (rug-pull'ları bloke eder)

---
**Branch:** feat/perf-rpc-quote-cache
**Commits:** 3 (perf + strategy + docs)
