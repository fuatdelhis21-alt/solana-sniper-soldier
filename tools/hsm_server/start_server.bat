@echo off
cd /d C:\Users\Lenovo\Downloads\solana-hft-platform

REM HSM_KEY_B64, production'da gerçek 64-byte private key'in base64'ü ile
REM environment variable olarak set edilmelidir. Bu dosyaya asla gerçek
REM secret yazmayın (dosya commit edilir). Set edilmemişse sunucu BASLAMAZ
REM (ephemeral / uretilmis anahtar fallback'i yoktur).
if "%HSM_KEY_B64%"=="" (
    echo [ERROR] HSM_KEY_B64 set edilmedi - sunucu anahtarsiz baslamaz (fail-closed)
    exit /b 1
)

"C:\Users\Lenovo\.cargo\bin\cargo.exe" run -p tools-hsm-server -- --certs tools/hsm_server/certs --log-file logs/hsm_audit.log
