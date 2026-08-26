# Windows Build Setup for Solana HFT Platform

## Prerequisite: Install OpenSSL (pre-built binaries)

`openssl-sys` crate requires a pre-installed OpenSSL on Windows.
The `vendored` feature does **not** work on Windows MSVC targets for `openssl-sys v0.9.117`.

### Option 1: Install via vcpkg (recommended)

```powershell
# Clone vcpkg (one-time)
cd C:\dev
git clone https://github.com/Microsoft/vcpkg.git
cd vcpkg
.\bootstrap-vcpkg.bat

# Install OpenSSL for x64 Windows
.\vcpkg install openssl:x64-windows

# Set env vars (add to your PowerShell profile or run before build)
$env:OPENSSL_DIR = "C:\dev\vcpkg\installed\x64-windows"
```

### Option 2: Download pre-compiled binaries from Shining Light Productions

1. Go to: https://slproweb.com/products/Win32OpenSSL.html
2. Download **Win64 OpenSSL v3.x** (Light version is enough)
3. Install to `C:\OpenSSL-Win64` (default)
4. Set environment:
```powershell
$env:OPENSSL_DIR = "C:\OpenSSL-Win64"
$env:OPENSSL_LIB_DIR = "C:\OpenSSL-Win64\lib\VC\x64\MT"
$env:OPENSSL_INCLUDE_DIR = "C:\OpenSSL-Win64\include"
```

### Option 3: Use Rustls-based RPC client (alternative - requires code changes)

If you cannot install OpenSSL, we can switch `solana-client` to use `rustls` instead.
This requires changing the workspace dependencies.

---

## Verify OpenSSL is detected

```powershell
$env:OPENSSL_DIR = "C:\dev\vcpkg\installed\x64-windows"  # adjust path
echo "OPENSSL_DIR=$env:OPENSSL_DIR"
```

Then run:

```powershell
cd C:\Users\Lenovo\Downloads\solana-hft-platform
cargo clean
cargo build -p solana-sniper
```

Expected output: build succeeds, produces `target\debug\send_transfer.exe`

---

## Next: Blackbox Canary Testing

Once build succeeds, run the blackbox runner:

```powershell
.\blackbox_runner.ps1
```

This will:
1. Build `send_transfer`
2. Execute `--dry-run` (simulation)
3. Print JSON result with signature/blockhash/logs
4. Prompt you to proceed with live transfer (`--live`)

---

## Production Deployment Checklist

- [ ] OpenSSL installed (this step)
- [ ] `cargo build -p solana-sniper` succeeds
- [ ] `blackbox_runner.ps1 --dry-run` passes
- [ ] Wallet funded with SOL on devnet (for live test)
- [ ] Live transfer verified on devnet
- [ ] Configure `config.toml` with mainnet RPC endpoints
- [ ] Switch to mainnet wallet
- [ ] Run full blackbox suite on mainnet (test trade)
