$ErrorActionPreference = 'Stop'

$solanaKeygen = Join-Path $env:USERPROFILE '.local\share\solana\bin\solana-keygen.exe'
$secureDir = Join-Path $env:USERPROFILE '.secure-hsm'
$keyPath = Join-Path $secureDir 'hsm_signing_key.json'
$repoRoot = (Resolve-Path $PSScriptRoot).Path
$walletPath = Join-Path $repoRoot 'wallet.json'

if (-not (Test-Path $solanaKeygen)) {
    throw "solana-keygen not found at $solanaKeygen"
}

New-Item -ItemType Directory -Force -Path $secureDir | Out-Null
if (-not (Test-Path $walletPath)) {
    throw "Wallet keypair not found at $walletPath"
}

# The HSM signer must be the funded wallet. A newly generated key would make
# pubkey verification fail and could never authorize the intended account.
Copy-Item -LiteralPath $walletPath -Destination $keyPath -Force

$publicKey = (& $solanaKeygen pubkey $keyPath).Trim()
$env:HSM_KEY_B64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($keyPath))

Write-Host "key_path=$keyPath"
Write-Host "public_key=$publicKey"
Write-Host 'HSM_KEY_B64 is set only in this process and is not written to disk.'