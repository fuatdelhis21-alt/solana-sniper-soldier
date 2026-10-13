#!/usr/bin/env bash
# Build a deployable release artifact (binaries + systemd units + config
# templates + install/rollback scripts) and optionally upload it to S3.
#   ./scripts/build_and_publish.sh            # builds target/release first
#   SKIP_BUILD=1 ./scripts/build_and_publish.sh
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT_DIR"

if [ "${SKIP_BUILD:-0}" != 1 ]; then
  cargo build --release -p solana-sniper -p tools-hsm-server
fi

STAMP=$(date +%Y%m%d%H%M%S)
ARTIFACT_NAME=solana-hft-release-$STAMP.tar.gz
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/bin" "$STAGE/systemd"
for b in solana-sniper pool-sniper hsm_server send_transfer; do
  install -m 0755 "target/release/$b" "$STAGE/bin/$b"
done
cp deploy/systemd/*.service "$STAGE/systemd/"
cp deploy/install_release.sh deploy/rollback.sh deploy/hsm.env.example "$STAGE/"
cp .env.example "$STAGE/solana-hft.env.example"
cp tools/hsm_server/certs/generate_certs.sh README_DEPLOY.md "$STAGE/"
cp scripts/sniper_report.py scripts/screen_candidates.py "$STAGE/"
git rev-parse HEAD > "$STAGE/REVISION" 2>/dev/null || true
(cd "$STAGE" && find . -type f ! -name SHA256SUMS -printf '%P\n' | sort | xargs sha256sum > SHA256SUMS)

echo "Packaging $ARTIFACT_NAME..."
tar -czf "$ARTIFACT_NAME" -C "$STAGE" .

if [ -n "${S3_ENDPOINT:-}" ] && [ -n "${S3_BUCKET:-}" ]; then
  echo "Uploading artifact to S3..."
  aws --endpoint-url "$S3_ENDPOINT" s3 cp "$ARTIFACT_NAME" "s3://$S3_BUCKET/$ARTIFACT_NAME"
else
  echo "S3_ENDPOINT/S3_BUCKET not set, skipping upload"
fi

echo "Done: $ARTIFACT_NAME"
