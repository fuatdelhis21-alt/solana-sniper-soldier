<#
.SYNOPSIS
  Enhanced blackbox runner for Solana HFT Phase1 canary testing.

.DESCRIPTION
  - Ensures crates/hft-marketdata has byteorder in Cargo.toml (adds or creates).
  - Ensures solana_ws.rs has ReadBytesExt + tracing imports (prepends if missing).
  - Runs cargo clean + cargo build -p solana-sniper
  - Runs send_transfer in --dry-run by default, saves logs and JSON summary.
  - Use -ForceSend to actually send (be careful).

.PARAMETER WorkspacePath
  Path to the workspace root (default: current directory).

.PARAMETER WalletPath
  Relative or absolute path to wallet.json (default: ./wallet.json inside workspace).

.PARAMETER To
  Destination public key (string).

.PARAMETER Amount
  Amount in SOL to send (float). Default 0.01

.PARAMETER Rpc
  RPC endpoint (default https://api.devnet.solana.com).

.PARAMETER ForceSend
  If present, skip --dry-run and actually send the transaction.

.PARAMETER SkipBuild
  If present, skip cargo build step (use existing binary).

.PARAMETER JitoTip
  Optional Jito tip in SOL (default 0.0). Only used if -ForceSend is set.

.EXAMPLE
  .\blackbox_runner.ps1 -WorkspacePath "C:\Users\Lenovo\Downloads\solana-hft-platform" -WalletPath "./wallet.json" -To "H1P..." -Amount 0.01

.EXAMPLE
  .\blackbox_runner.ps1 -To "H1P..." -ForceSend -Amount 0.001
#>

param(
  [string]$WorkspacePath = (Get-Location).Path,
  [string]$WalletPath = "./wallet.json",
  [Parameter(Mandatory=$true)][string]$To,
  [double]$Amount = 0.01,
  [string]$Rpc = "https://api.devnet.solana.com",
  [switch]$ForceSend,
  [switch]$SkipBuild,
  [double]$JitoTip = 0.0
)

function Write-Log { param($m); $ts=(Get-Date).ToString("o"); $line="$ts`t$m"; Write-Host $line; Add-Content -Path $global:logFile -Value $line }

# --- prepare logging & paths ---
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
$logsDir = Join-Path $WorkspacePath "data\blackbox_logs"
if (-not (Test-Path $logsDir)) { New-Item -ItemType Directory -Path $logsDir -Force | Out-Null }
$global:logFile  = Join-Path $logsDir "blackbox_patch_$timestamp.log"
$outFile = Join-Path $logsDir "blackbox_patch_$timestamp.out"
$errFile = Join-Path $logsDir "blackbox_patch_$timestamp.err"
$resultFile = Join-Path $logsDir "blackbox_patch_result_$timestamp.json"

"--- BLACKBOX PATCH RUN START $timestamp ---" | Out-File $global:logFile
Write-Log "Workspace: $WorkspacePath"
Write-Log "Wallet: $WalletPath"
Write-Log "Dest: $To"
Write-Log "Amount: $Amount"
Write-Log "RPC: $Rpc"
Write-Log "ForceSend: $ForceSend"
Write-Log "JitoTip: $JitoTip"

# validate workspace
if (-not (Test-Path $WorkspacePath)) { Write-Log "ERROR: WorkspacePath not found: $WorkspacePath"; exit 2 }

# resolve absolute paths
if ([System.IO.Path]::IsPathRooted($WalletPath)) { $absWallet = $WalletPath }
else { $absWallet = Join-Path $WorkspacePath $WalletPath }
if (-not (Test-Path $absWallet)) { Write-Log "ERROR: wallet.json not found at $absWallet"; exit 3 }

# ===================================================================
# PATCH 1: Ensure crates/hft-marketdata/Cargo.toml has byteorder
# ===================================================================
Write-Log "--- PATCH 1: byteorder in hft-marketdata Cargo.toml ---"
$crateToml = Join-Path $WorkspacePath "crates\hft-marketdata\Cargo.toml"
if (-not (Test-Path $crateToml)) {
  Write-Log "WARNING: Cargo.toml for hft-marketdata not found. Creating minimal Cargo.toml at $crateToml"
  $dir = Split-Path $crateToml -Parent
  if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
@"
[package]
name = "hft-marketdata"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
solana-client = { workspace = true }
solana-sdk = { workspace = true }
solana-rpc-client = { workspace = true }
solana-rpc-client-api = { workspace = true }
solana-account-decoder = { workspace = true }
solana-transaction = { workspace = true }
solana-pubkey = { workspace = true }
parking_lot = { workspace = true }
thiserror = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
byteorder = "1.5"
futures-util = "0.3"
tokio-tungstenite = { version = "0.24", features = ["native-tls"] }
futures = "0.3"
base64 = "0.22"
"@ | Out-File -FilePath $crateToml -Encoding UTF8
  Write-Log "Created minimal Cargo.toml with byteorder"
} else {
  $toml = Get-Content $crateToml -Raw
  if ($toml -notmatch "byteorder\s*=") {
    Write-Log "byteorder not present — injecting..."
    # inject after [dependencies] line
    $newToml = $toml -replace '(\[dependencies\])', "`$1`nbyteorder = `"1.5`""
    # fallback: append to end
    if ($newToml -eq $toml) {
      $newToml = $toml.TrimEnd() + "`nbyteorder = `"1.5`"`n"
    }
    $newToml | Out-File -FilePath $crateToml -Encoding UTF8
    Write-Log "Injected byteorder dependency"
  } else {
    Write-Log "byteorder already present, OK"
  }
}

# ===================================================================
# PATCH 2: Ensure solana_ws.rs has ReadBytesExt + tracing imports
# ===================================================================
Write-Log "--- PATCH 2: imports in solana_ws.rs ---"
$wsFile = Join-Path $WorkspacePath "crates\hft-marketdata\src\solana_ws.rs"
if (Test-Path $wsFile) {
  $content = Get-Content $wsFile -Raw
  $needsUpdate = $false

  if ($content -notmatch "use byteorder") {
    Write-Log "ReadBytesExt import missing — prepending..."
    $content = "use byteorder::{LittleEndian, ReadBytesExt};`r`n" + $content
    $needsUpdate = $true
  }

  if ($content -notmatch "use tracing") {
    Write-Log "tracing import missing — prepending..."
    $content = "use tracing;`r`n" + $content
    $needsUpdate = $true
  }

  if ($needsUpdate) {
    $content | Out-File -FilePath $wsFile -Encoding UTF8
    Write-Log "Imports patched"
  } else {
    Write-Log "All imports present, OK"
  }
} else {
  Write-Log "WARNING: solana_ws.rs not found at $wsFile — cannot patch imports"
}

# ===================================================================
# STEP 1: cargo clean + cargo build -p solana-sniper
# ===================================================================
if (-not $SkipBuild) {
  Write-Log "--- BUILD STEP ---"

  Write-Log "Running: cargo clean -p solana-sniper"
  & cargo clean --manifest-path (Join-Path $WorkspacePath "Cargo.toml") -p solana-sniper 2>&1 | Out-File -FilePath $errFile -Append

  Write-Log "Running: cargo build -p solana-sniper"
  $buildOk = $true
  try {
    & cargo build --manifest-path (Join-Path $WorkspacePath "Cargo.toml") -p solana-sniper 2>&1 | Out-File -FilePath $errFile -Append
    if ($LASTEXITCODE -ne 0) { $buildOk = $false }
  } catch {
    Write-Log "ERROR in cargo build: $_"
    $buildOk = $false
  }

  if (-not $buildOk) {
    Write-Log "BUILD FAILED — see $errFile for details"
    $buildExit = 1
  } else {
    Write-Log "BUILD SUCCEEDED"
    $buildExit = 0
  }
} else {
  Write-Log "--- SKIP BUILD (SkipBuild flag) ---"
  $buildExit = 0
}

# ===================================================================
# STEP 2: run send_transfer
# ===================================================================
Write-Log "--- SEND_TRANSFER STEP ---"

$dryRunFlag = if ($ForceSend) { "" } else { "--dry-run" }

$runArgs = @(
  "run", "--manifest-path", (Join-Path $WorkspacePath "Cargo.toml"),
  "--bin", "send_transfer", "--",
  "--rpc", $Rpc,
  "--wallet", $absWallet,
  "--to", $To,
  "--amount", ([string]$Amount)
)
if ($dryRunFlag) { $runArgs += $dryRunFlag }

Write-Log "Running: cargo run --bin send_transfer $($runArgs[6..$runArgs.Count] -join ' ')"
$runExit = 0
try {
  & cargo $runArgs 2>&1 | Tee-Object -FilePath $outFile
  $runExit = $LASTEXITCODE
} catch {
  Write-Log "ERROR in cargo run: $_"
  $runExit = 1
}

# ===================================================================
# STEP 3: collect outputs & parse results
# ===================================================================
$stdout = ""
$stderr = ""
if (Test-Path $outFile) { $stdout = Get-Content $outFile -Raw }
if (Test-Path $errFile) { $stderr = Get-Content $errFile -Raw }

# Parse signature from stdout (JSON or plain text)
$signature = $null
$explorerUrl = $null

# Try JSON output first (from send_transfer)
if ($stdout -match '"signature"\s*:\s*"([^"]+)"') {
  $signature = $matches[1]
}
if ($stdout -match '"explorer"\s*:\s*"([^"]+)"') {
  $explorerUrl = $matches[1]
}

# Fallback: plain text signature line
if (-not $signitude) {
  if ($stdout -match "Signature:\s*(\S+)") { $signature = $matches[1].Trim() }
  if ($stdout -match "TxID:\s*(\S+)")     { $signature = $matches[1].Trim() }
  if ($stdout -match "Explorer:\s*(\S+)") { $explorerUrl = $matches[1].Trim() }
}

# ===================================================================
# STEP 4: write JSON summary
# ===================================================================
$result = @{
  timestamp    = $timestamp
  workspace    = $WorkspacePath
  wallet       = $absWallet
  to           = $To
  amount       = $Amount
  rpc          = $Rpc
  force_send   = [bool]$ForceSend
  jito_tip     = $JitoTip
  build_exit   = $buildExit
  run_exit     = $runExit
  signature    = $signature
  explorer_url = $explorerUrl
  stdout       = ($stdout -replace "`r","")
  stderr       = ($stderr -replace "`r","")
}

$json = $result | ConvertTo-Json -Depth 6
$json | Out-File -FilePath $resultFile -Encoding utf8

Write-Log "Blackbox run finished. Build exit: $buildExit, Run exit: $runExit"
if ($signature) { Write-Log "SIGNATURE: $signature" }
if ($explorerUrl) { Write-Log "EXPLORER: $explorerUrl" }
Write-Log "Result JSON: $resultFile"

# --- print summary ---
Write-Host "`n===== BLACKBOX RUN SUMMARY ====="
Write-Host ($json | ConvertFrom-Json | Format-List | Out-String)
Write-Host "================================"

exit $runExit
