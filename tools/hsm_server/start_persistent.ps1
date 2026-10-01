$ErrorActionPreference = 'Stop'

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$keyPath = Join-Path $env:USERPROFILE '.secure-hsm\hsm_signing_key.json'
$certsPath = Join-Path $repoRoot 'tools\hsm_server\certs'
$logPath = Join-Path $repoRoot 'logs\hsm_audit.log'
$serverPath = Join-Path $repoRoot 'target\debug\hsm_server.exe'

if (-not (Test-Path $keyPath)) {
    throw "Persistent HSM key not found: $keyPath"
}
if (-not (Test-Path $serverPath)) {
    throw "HSM server binary not found: $serverPath. Run cargo build -p tools-hsm-server first."
}

$keyBytes = [byte[]](Get-Content $keyPath -Raw | ConvertFrom-Json)
if ($keyBytes.Count -ne 64) {
    throw "Persistent HSM key must contain exactly 64 bytes."
}

$env:HSM_KEY_B64 = [Convert]::ToBase64String($keyBytes)
Set-Location $repoRoot
& $serverPath --certs $certsPath --log-file $logPath