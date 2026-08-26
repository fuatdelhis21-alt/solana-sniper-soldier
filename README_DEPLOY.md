**Deployment guide**

Quick steps to build, test, package and deploy to bare-metal using Ansible and GitHub Actions.

1. Locally build & test

```bash
cargo build --release
cargo test --all
```

2. Create production config from example

```bash
cp config/production.example.toml /etc/solana-hft/config.toml
# populate secrets via Vault or environment variables
```

3. Run Ansible deploy (example)

```bash
export ARTIFACT_PATH=/path/to/solana-hft-YYYYMMDDHHMMSS.tar.gz
ansible-playbook -i inventory.ini ansible/deploy.yml -u ubuntu
```

4. GitHub Actions:

- Configure secrets: `S3_ENDPOINT`, `S3_BUCKET`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE`
- Push to `main` to trigger CI/CD
