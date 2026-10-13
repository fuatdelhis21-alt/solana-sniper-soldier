#!/usr/bin/env bash
# Linux/macOS equivalent of generate_certs.ps1: full mTLS PKI for the Remote
# HSM signer. Outputs (in the target directory, default: this script's dir):
#   ca.pem / ca.key          root CA
#   server.pem / server.key  server cert (SAN: localhost, 127.0.0.1, + $HSM_SAN)
#   client.pem / client.key  client cert (extendedKeyUsage=clientAuth)
#   client_all.pem           combined client cert + key (what the bot loads)
# Usage: ./generate_certs.sh [out_dir]
# Extra server SANs: HSM_SAN="DNS:hsm.internal,IP:10.0.0.5" ./generate_certs.sh
set -euo pipefail

OUT="${1:-$(cd "$(dirname "$0")" && pwd)}"
mkdir -p "$OUT"
cd "$OUT"
umask 077

SAN="DNS:localhost,IP:127.0.0.1"
if [ -n "${HSM_SAN:-}" ]; then SAN="$SAN,$HSM_SAN"; fi

rm -f ca.key ca.pem server.key server.csr server.pem server.crt \
      client.key client.csr client.pem client_all.pem ca.srl

openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
  -keyout ca.key -out ca.pem -subj "/C=US/O=SolanaHFT/CN=SolanaHFT-Root-CA" 2>/dev/null

openssl req -newkey rsa:2048 -sha256 -nodes -keyout server.key -out server.csr \
  -subj "/C=US/O=SolanaHFT/CN=localhost" 2>/dev/null
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out server.pem -days 825 -sha256 \
  -extfile <(printf "subjectAltName=%s\n" "$SAN") 2>/dev/null
cp server.pem server.crt

openssl req -newkey rsa:2048 -sha256 -nodes -keyout client.key -out client.csr \
  -subj "/C=US/O=SolanaHFT/CN=hft-client" 2>/dev/null
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out client.pem -days 825 -sha256 \
  -extfile <(printf "extendedKeyUsage=clientAuth\n") 2>/dev/null

cat client.pem client.key > client_all.pem
rm -f server.csr client.csr ca.srl

echo "mTLS PKI written to $OUT (server SAN: $SAN)"
ls -l ca.pem server.pem server.key client.pem client.key client_all.pem
