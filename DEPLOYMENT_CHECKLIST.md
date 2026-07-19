# Solana HFT Platform — Deployment Checklist

## ✅ Tamamlanan

1. **Project Setup & CI/CD Scaffold**
   - ✅ Workspace manifest (`Cargo.toml`)
   - ✅ GitHub Actions workflow (`.github/workflows/ci-cd.yml`)
   - ✅ Build & publish script (`scripts/build_and_publish.sh`)
   - ✅ Production config template (`config/production.example.toml`)

2. **Deployment Infrastructure**
   - ✅ Ansible playbook (`ansible/deploy.yml`)
   - ✅ systemd unit template (`ansible/templates/solana-hft.service.j2`)
   - ✅ Install script (`deploy/install_release.sh`)
   - ✅ README_DEPLOY.md

3. **Build & Test Preparation**
   - ✅ Workspace members configured (hft-core, hft-marketdata, hft-execution)
   - ✅ Mock artifact created: `solana-hft-release-mock-20260716152808.tar.gz`

## 📋 Sonraki Adımlar (Priority Order)

### 1. GitHub Actions ile Real Build (⏳ En Önemli)
```bash
# Prerequisites:
# - Git repo already pushed to https://github.com/fuatdelhis21-alt/solana-sniper-soldier
# - GitHub Secrets configured (CI'da gerekli):
#   - S3_ENDPOINT
#   - S3_BUCKET
#   - AWS_ACCESS_KEY_ID
#   - AWS_SECRET_ACCESS_KEY
#   - (optional) GPG_PRIVATE_KEY + GPG_PASSPHRASE

# Action: Push to main branch (veya manual workflow_dispatch tetikle)
git push -u origin main
# → Actions kart başlayacak, build/test/artifact üretecek
```

### 2. Bare-Metal Deploy (Ansible) — SSH Gerekli
```bash
# Host prep (target sunucuda, root olarak):
useradd -r -s /sbin/nologin solana || true
mkdir -p /etc/solana-hft /opt/solana-hft
cp config/production.example.toml /etc/solana-hft/config.toml
# ... production env değerlerini Vault/export ile ekle ...

# Deploy (control node'dan):
export ARTIFACT_PATH=/path/to/solana-hft-release-YYYYMMDDHHMMSS.tar.gz
ansible-playbook -i inventory.ini ansible/deploy.yml -u ubuntu -k

# Verify:
systemctl status solana-hft
journalctl -u solana-hft -f
```

### 3. systemd Service Validation
```bash
# Bare-metal üzerinde:
cat /etc/systemd/system/solana-hft.service
systemctl enable solana-hft
systemctl start solana-hft
systemctl restart solana-hft  # graceful reload test
```

### 4. Monitoring & Rollback (Production)
```bash
# Monitoring (Prometheus scrape):
curl http://localhost:9090/metrics  # (TBD: port, endpoint)

# Rollback (previous artifact backup):
tar -czf backup-$(date +%s).tar.gz -C /opt/solana-hft .
./deploy/install_release.sh /path/to/backup-release.tar.gz

# Kill-switch / circuit breaker:
systemctl stop solana-hft
systemctl disable solana-hft
```

---

## 📌 Gerekli Bilgiler (Eksik)

- [ ] SSH Public Key (bare-metal için)
- [ ] Inventory File (`inventory.ini` — host IPs, credentials)
- [ ] Vault Token veya env secrets (production config için)
- [ ] S3/Artifact Registry Credentials (CI için)
- [ ] Rollback Policy (automatic vs. manual)

---

## 🚀 Quick Start (Test Amaçlı)

### Option A: GitHub Actions ile (Gerçek)
```bash
cd C:\Users\Lenovo\Downloads\solana-hft-platform
git push -u origin main
# → Watch Actions tab: github.com/fuatdelhis21-alt/solana-sniper-soldier/actions
```

### Option B: Lokal Mock Deploy (Test)
```bash
ansible-playbook ansible/deploy.yml -i localhost, \
  -e "artifact_path=./solana-hft-release-mock-20260716152808.tar.gz" \
  --check  # dry-run
```

### Option C: Manual Deploy (Bare-Metal)
```bash
./deploy/install_release.sh solana-hft-release-mock-20260716152808.tar.gz config/production.example.toml
```

---

## ⚠️ Critical Notes

1. **Build**: Windows linker (`link.exe`) yok — GitHub Actions veya WSL/Linux kullanın.
2. **Secrets**: Hiçbir zaman plaintext secret'ı git'e commit etmeyin — GitHub Secrets veya Vault kullanın.
3. **SSH**: Bare-metal deploy için SSH public key ekle ve `ansible-inventory` yapılandır.
4. **Service**: `systemd` user/group `solana:solana` olmalı; perms 0755 bin, 0600 config.
5. **Rollback**: Gönderimi önceki artefakt backup'ını her zaman sakla.

---

**Status**: ✅ Infrastructure Ready | ⏳ Awaiting Secrets + Build | ⏳ Deploy pending SSH setup
