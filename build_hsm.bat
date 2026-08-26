@echo off
cd /d C:\Users\Lenovo\Downloads\solana-hft-platform
set "OPENSSL_DIR=C:\Users\Lenovo\vcpkg\installed\x64-windows"
cargo build --manifest-path tools/hsm_server/Cargo.toml > "C:\Users\Lenovo\Desktop\build_hsm_new.txt" 2>&1
echo BUILD_EXIT=%ERRORLEVEL% >> "C:\Users\Lenovo\Desktop\build_hsm_new.txt"
