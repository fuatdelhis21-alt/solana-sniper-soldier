@echo off
REM Solana HFT Platform — Devnet Setup Script
REM Run this to create a devnet wallet and airdrop SOL

echo ========================================
echo  Solana HFT Platform — Devnet Setup
echo ========================================
echo.

REM Create wallet.json (devnet)
echo [1/3] Creating wallet.json...
if exist wallet.json (
    echo   wallet.json already exists. Delete it first if you want a new one.
) else (
    solana-keygen new --outfile wallet.json --no-passphrase --force
    if %errorlevel% neq 0 (
        echo   ERROR: solana-keygen not found. Install Solana CLI first.
        exit /b 1
    )
    echo   wallet.json created.
)

REM Get pubkey
echo.
echo [2/3] Getting public key...
for /f "delims=" %%i in ('solana-keygen pubkey wallet.json') do set PUBKEY=%%i
echo   Pubkey: %PUBKEY%

REM Airdrop devnet SOL
echo.
echo [3/3] Airdropping 2 devnet SOL...
solana airdrop 2 %PUBKEY% --url https://api.devnet.solana.com
if %errorlevel% neq 0 (
    echo   WARNING: Airdrop may have failed. Check balance manually.
) else (
    echo   Airdrop successful!
)

REM Show balance
echo.
echo Balance:
solana balance %PUBKEY% --url https://api.devnet.solana.com

echo.
echo ========================================
echo  Setup complete!
echo  Next: run simulation: cargo run --bin solana-sniper -- --iterations 10
echo  Or dry-run: cargo run --bin solana-sniper -- --wallet ./wallet.json --dry-run --iterations 5
echo ========================================
pause
