# verify_mainnet_ready.ps1
# Mainnet öncesi doğrulama scripti.
# SIR GÖSTERMEZ: yalnızca varlık/bakiye/endpoint kontrolü yapar.
# Kullanım: .\scripts\verify_mainnet_ready.ps1 -Rpc https://api.mainnet-beta.solana.com -Pubkey <MAINNET_PUBKEY>
#
# NOT: Bu script hiçbir işlem göndermez, yalnızca salt-okunur kontroller yapar.

param(
    [string]$Rpc = "https://api.mainnet-beta.solana.com",
    [string]$Pubkey = ""
)

$ErrorActionPreference = "Stop"
$fail = 0

Write-Host "=== Mainnet Oncesi Dogrulama ===" -ForegroundColor Cyan
Write-Host "RPC: $Rpc"
Write-Host ""

# 1. RPC endpoint erisilebilirlik + cluster dogrulama
Write-Host "[1] RPC endpoint kontrolu..." -ForegroundColor Yellow
try {
    $body = @{ jsonrpc="2.0"; id=1; method="getVersion"; params=@() } | ConvertTo-Json
    $resp = Invoke-RestMethod -Uri $Rpc -Method Post -ContentType "application/json" -Body $body -TimeoutSec 15
    Write-Host "    OK: solana-core $($resp.result['solana-core'])" -ForegroundColor Green
} catch {
    Write-Host "    HATA: RPC'ye ulasilamadi: $($_.Exception.Message)" -ForegroundColor Red
    $fail++
}

# 2. Cluster tipi dogrulama (mainnet-beta olmali)
Write-Host "[2] Cluster tipi kontrolu..." -ForegroundColor Yellow
try {
    $body = @{ jsonrpc="2.0"; id=1; method="getGenesisHash"; params=@() } | ConvertTo-Json
    $resp = Invoke-RestMethod -Uri $Rpc -Method Post -ContentType "application/json" -Body $body -TimeoutSec 15
    $genesis = $resp.result
    # Mainnet-beta genesis hash'i
    if ($genesis -eq "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d") {
        Write-Host "    OK: mainnet-beta dogrulandi" -ForegroundColor Green
    } else {
        Write-Host "    UYARI: genesis hash mainnet-beta ile eslesmiyor: $genesis" -ForegroundColor Red
        $fail++
    }
} catch {
    Write-Host "    HATA: genesis hash alinamadi: $($_.Exception.Message)" -ForegroundColor Red
    $fail++
}

# 3. Cuzdan bakiyesi (sir gostermeden, sadece bakiye)
if ($Pubkey) {
    Write-Host "[3] Cuzdan bakiyesi kontrolu..." -ForegroundColor Yellow
    try {
        $body = @{ jsonrpc="2.0"; id=1; method="getBalance"; params=@($Pubkey) } | ConvertTo-Json
        $resp = Invoke-RestMethod -Uri $Rpc -Method Post -ContentType "application/json" -Body $body -TimeoutSec 15
        $lamports = $resp.result.value
        $sol = [math]::Round($lamports / 1e9, 4)
        Write-Host "    Pubkey: $Pubkey" -ForegroundColor Gray
        Write-Host "    Bakiye: $lamports lamports = $sol SOL" -ForegroundColor Green
        if ($lamports -lt 10000000) { # < 0.01 SOL
            Write-Host "    UYARI: Bakiye cok dusuk, islem ucretlerini karsilamayabilir" -ForegroundColor Red
            $fail++
        }
    } catch {
        Write-Host "    HATA: Bakiye alinamadi: $($_.Exception.Message)" -ForegroundColor Red
        $fail++
    }
} else {
    Write-Host "[3] Cuzdan bakiyesi: -Pubkey parametresi verilmedi, atlandi" -ForegroundColor Gray
}

# 4. HSM_KEY_B64 varligi (degeri gostermeden)
Write-Host "[4] HSM_KEY_B64 varligi..." -ForegroundColor Yellow
if ($env:HSM_KEY_B64) {
    Write-Host "    OK: HSM_KEY_B64 set edilmis (deger gosterilmez)" -ForegroundColor Green
} else {
    Write-Host "    HATA: HSM_KEY_B64 set edilmemis! Mainnet icin gerekli." -ForegroundColor Red
    $fail++
}

# 5. mTLS sertifika dosyalari varligi
Write-Host "[5] mTLS sertifika dosyalari..." -ForegroundColor Yellow
$certs = @("tools/hsm_server/certs/ca.pem", "tools/hsm_server/certs/client_all.pem")
foreach ($c in $certs) {
    if (Test-Path $c) {
        Write-Host "    OK: $c mevcut" -ForegroundColor Green
    } else {
        Write-Host "    HATA: $c bulunamadi" -ForegroundColor Red
        $fail++
    }
}

Write-Host ""
if ($fail -eq 0) {
    Write-Host "SONUC: TUM KONTROLLER GECTI. Mainnet oncesi hazir." -ForegroundColor Green
    exit 0
} else {
    Write-Host "SONUC: $fail kontrol BASARISIZ. Mainnet'e gecmeyin, oncelikle duzeltin." -ForegroundColor Red
    exit 1
}
