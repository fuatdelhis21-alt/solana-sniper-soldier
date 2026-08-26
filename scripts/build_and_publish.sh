#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT_DIR"

TARGET_DIR=target/release
ARTIFACT_NAME=solana-hft-platform-$(date +%Y%m%d%H%M%S).tar.gz

echo "Packaging release artifacts..."
tar -czf "$ARTIFACT_NAME" -C "$TARGET_DIR" .

if [ -n "${S3_ENDPOINT:-}" ]; then
  echo "Uploading artifact to S3..."
  aws --endpoint-url "$S3_ENDPOINT" s3 cp "$ARTIFACT_NAME" "s3://$S3_BUCKET/$ARTIFACT_NAME"
else
  echo "S3_ENDPOINT not set, skipping upload"
fi

echo "Done: $ARTIFACT_NAME"
