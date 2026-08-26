@echo off
cd /d C:\Users\Lenovo\Downloads\solana-hft-platform
set "OPENSSL_DIR=C:\Users\Lenovo\vcpkg\installed\x64-windows"
echo === BUILDING solana-sniper (client adapter) ===
call cargo build -p solana-sniper > C:\Users\Lenovo\Desktop\build_remotehsm.txt 2>&1
echo BUILD_EXIT=%ERRORLEVEL%
