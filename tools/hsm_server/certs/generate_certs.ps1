# =====================================================================
# generate_certs.ps1
#
# Generates a full PKI for the Remote HSM Signer server with mutual TLS (mTLS).
#
# Outputs (in this directory):
#   ca.pem        - self-signed root CA certificate (PEM)
#   ca.key        - CA private key (PEM)  [keep secure]
#   server.pem    - server certificate chain (PEM, cert only, signed by CA)
#   server.key    - server private key (PEM, 2048-bit RSA)
#   client.pem    - client certificate (PEM, signed by CA)
#   client.key    - client private key (PEM, 2048-bit RSA)
#   server.crt    - alias of server.pem (for compatibility)
#
# The server enforces mTLS: it requires clients to present a certificate
# signed by ca.pem. The client uses client.pem + client.key to authenticate.
#
# Requirements:
#   - OpenSSL available on PATH, OR
#   - Git for Windows (bundles openssl)
# =====================================================================

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $here

Write-Host "=== HSM Server mTLS PKI Generation ===" -ForegroundColor Cyan

# ---------------------------------------------------------------------
# 1. Locate OpenSSL
# ---------------------------------------------------------------------
$openssl = $null

$onPath = Get-Command "openssl" -ErrorAction SilentlyContinue
if ($onPath) { $openssl = $onPath.Source }

if (-not $openssl) {
  $gitBin = "C:\Program Files\Git\usr\bin\openssl.exe"
  if (Test-Path $gitBin) { $openssl = $gitBin }
}

if (-not $openssl) {
  $candidates = @(
    "C:\Program Files\OpenSSL\bin\openssl.exe",
    "C:\Program Files (x86)\OpenSSL\bin\openssl.exe",
    "$env:LOCALAPPDATA\Programs\Git\usr\bin\openssl.exe",
    "$env:ChocolateyInstall\bin\openssl.exe"
  )
  foreach ($c in $candidates) {
    if (Test-Path $c) { $openssl = $c; break }
  }
}

if (-not $openssl) {
  Write-Host "[FAIL] OpenSSL not found. Install OpenSSL or Git for Windows." -ForegroundColor Red
  exit 1
}
Write-Host "[i] Using OpenSSL: $openssl" -ForegroundColor Green

# ---------------------------------------------------------------------
# 2. File paths
# ---------------------------------------------------------------------
$caKey    = Join-Path $here "ca.key"
$caPem    = Join-Path $here "ca.pem"
$srvKey   = Join-Path $here "server.key"
$srvCsr   = Join-Path $here "server.csr"
$srvPem   = Join-Path $here "server.pem"
$srvCrt   = Join-Path $here "server.crt"
$cliKey   = Join-Path $here "client.key"
$cliCsr   = Join-Path $here "client.csr"
$cliPem   = Join-Path $here "client.pem"

# Remove old artifacts
Remove-Item -Path $caKey, $caPem, $srvKey, $srvCsr, $srvPem, $srvCrt, $cliKey, $cliCsr, $cliPem -ErrorAction SilentlyContinue

$prevEAP = $ErrorActionPreference
$ErrorActionPreference = "Continue"

# ---------------------------------------------------------------------
# 3. CA certificate
# ---------------------------------------------------------------------
Write-Host "[i] Generating root CA key + cert..."
& $openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes `
    -keyout $caKey -out $caPem `
    -subj "/C=US/O=SolanaHFT/CN=SolanaHFT-Root-CA" 2>&1 | Out-Null

# ---------------------------------------------------------------------
# 4. Server certificate (signed by CA)
# ---------------------------------------------------------------------
Write-Host "[i] Generating server key + CSR..."
& $openssl req -newkey rsa:2048 -sha256 -nodes `
    -keyout $srvKey -out $srvCsr `
    -subj "/C=US/O=SolanaHFT/CN=localhost" `
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:0.0.0.0" 2>&1 | Out-Null

Write-Host "[i] Signing server cert with CA..."
$srvExt = Join-Path $here "server_ext.cnf"
@"
subjectAltName=DNS:localhost,IP:127.0.0.1,IP:0.0.0.0
"@ | Set-Content -Path $srvExt -Encoding ASCII
& $openssl x509 -req -in $srvCsr -CA $caPem -CAkey $caKey -CAcreateserial `
    -out $srvPem -days 825 -sha256 `
    -extfile $srvExt 2>&1 | Out-Null
Remove-Item $srvExt -ErrorAction SilentlyContinue

# ---------------------------------------------------------------------
# 5. Client certificate (signed by CA)
# ---------------------------------------------------------------------
Write-Host "[i] Generating client key + CSR..."
& $openssl req -newkey rsa:2048 -sha256 -nodes `
    -keyout $cliKey -out $cliCsr `
    -subj "/C=US/O=SolanaHFT/CN=hft-client" 2>&1 | Out-Null

Write-Host "[i] Signing client cert with CA..."
$cliExt = Join-Path $here "client_ext.cnf"
@"
extendedKeyUsage=clientAuth
"@ | Set-Content -Path $cliExt -Encoding ASCII
& $openssl x509 -req -in $cliCsr -CA $caPem -CAkey $caKey -CAcreateserial `
    -out $cliPem -days 825 -sha256 `
    -extfile $cliExt 2>&1 | Out-Null
Remove-Item $cliExt -ErrorAction SilentlyContinue

$ErrorActionPreference = $prevEAP

# ---------------------------------------------------------------------
# 6. Compatibility alias + combined client PEM + cleanup
# ---------------------------------------------------------------------
if ((Test-Path $srvPem) -and (Test-Path $srvKey)) {
  Copy-Item $srvPem $srvCrt -Force
}

# Build a combined client identity PEM (cert chain + private key in one file).
# reqwest::Identity::from_pem expects cert + key in a single PEM buffer.
$cliAll = Join-Path $here "client_all.pem"
if ((Test-Path $cliPem) -and (Test-Path $cliKey)) {
  $certContent = Get-Content $cliPem -Raw
  $keyContent  = Get-Content $cliKey -Raw
  ($certContent.TrimEnd() + "`n" + $keyContent) | Set-Content -Path $cliAll -Encoding ASCII
}
Remove-Item -Path $srvCsr, $cliCsr, "ca.srl" -ErrorAction SilentlyContinue

# ---------------------------------------------------------------------
# 7. Summary
# ---------------------------------------------------------------------
Write-Host ""
Write-Host "=== mTLS CERTIFICATES GENERATED ===" -ForegroundColor Cyan
foreach ($f in @($caPem, $caKey, $srvPem, $srvKey, $cliPem, $cliKey)) {
  if (Test-Path $f) {
    $leaf = Split-Path $f -Leaf
    $size = (Get-Item $f).Length
    Write-Host ("  " + $leaf + "  " + $size + " bytes") -ForegroundColor Green
  }
}

Write-Host ""
Write-Host "Server expects:" -ForegroundColor Cyan
Write-Host "  cert_path (server chain): certs/server.pem" -ForegroundColor Green
Write-Host "  key_path  (server key):   certs/server.key" -ForegroundColor Green
Write-Host "  client_auth (CA):         certs/ca.pem" -ForegroundColor Green
Write-Host ""
Write-Host "Client presents:" -ForegroundColor Cyan
Write-Host "  certificate: certs/client.pem" -ForegroundColor Green
Write-Host "  private key: certs/client.key" -ForegroundColor Green
Write-Host ""
Write-Host "For production, replace these with a real CA and keep private keys protected." -ForegroundColor Yellow
Write-Host "=== DONE ===" -ForegroundColor Green
