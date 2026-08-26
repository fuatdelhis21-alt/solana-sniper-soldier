    # TODO — mTLS Enforcement on Remote HSM Server

## Goal
Enforce mutual TLS on the HSM server so only authenticated clients (with a CA-signed client cert) can request signatures.

## Steps
- [x] 1. Update `tools/hsm_server/certs/generate_certs.ps1` to generate full PKI:
      - `ca.pem` + `ca.key` (root CA)
      - `server.pem` + `server.key` (server cert signed by CA, pure chain)
      - `client.pem` + `client.key` (client cert signed by CA)
      - `client_all.pem` (combined cert + key for `reqwest::Identity::from_pem`)
- [x] 2. Update `tools/hsm_server/main.rs` to enforce mTLS:
      - `cert_path("certs/server.pem")` (cert chain, no key)
      - `key_path("certs/server.key")`
      - `client_auth_required_path("certs/ca.pem")`
- [x] 3. Update `solana-sniper/src/remote_hsm.rs` client adapter:
      - Uses `use_rustls_tls()` (no OpenSSL dependency)
      - `new(endpoint, ca_cert_path, client_identity_path)` — reads `certs/ca.pem`
        + `certs/client_all.pem` at runtime and presents client identity via
        `reqwest::Identity::from_pem` for mTLS handshake.
- [x] 4. Add `rustls-tls` feature to workspace `reqwest` dependency.
- [x] 5. Regenerate certs by running the updated `generate_certs.ps1` (`client_all.pem` verified: cert + key).
- [x] 6. Rebuild HSM server + client (both clean; `remote_hsm` unit tests 2/2 pass).
- [x] 7. Test mTLS handshake (server up, client with cert succeeds, client without cert fails).
      - **Positive**: `solana-sniper --dry-run --hsm-endpoint https://127.0.0.1:8443 --hsm-ca certs/ca.pem --hsm-client-identity certs/client_all.pem`
        signed 3/3 transactions via the remote HSM (mTLS handshake OK). Server audit log recorded 6 signature requests.
      - **Negative**: `curl` without a client cert → TLS fatal alert (`SEC_E_ILLEGAL_MESSAGE`), request rejected (exit 56).
      - **Runtime fix**: `reqwest::blocking` builds an internal tokio runtime; ran HSM signing inside
        `tokio::task::spawn_blocking` to avoid "Cannot drop a runtime in a context where blocking is not allowed".
- [x] 8. Verify fail-closed behavior (HSM kapalıyken bot durur).
      - **Test**: `solana-sniper --live --dry-run --hsm-endpoint https://127.0.0.1:8443 --hsm-ca certs/ca.pem --hsm-client-identity certs/client_all.pem --iterations 1`
        while HSM server is stopped.
      - **Result**: `Error: "remote hsm request failed: error sending request for url (https://127.0.0.1:8443/sign)"`
        Bot exits with non-zero exit code. **No local fallback, no silent continue.** The error propagates through `??` in main.rs, causing immediate termination. ✅ PASS
- [x] 9. Add CI smoke test (`.github/workflows/hsm-mtls-smoke-test.yml`).
      - Builds all binaries on `ubuntu-latest` with stable Rust.
      - Generates mTLS PKI via `generate_certs.ps1` (pwsh).
      - Starts `hsm_server` in background with mTLS certs.
      - **Positive**: `solana-sniper --dry-run` signs via HSM → must exit 0.
      - **Negative**: `curl` without client cert → must be rejected (non-zero).
      - Checks `hsm_audit.log` for `request_id` + `signature_b64` entries.
- [x] 10. Add Operations Runbook (`docs/HSM_SERVER_RUNBOOK.md`).
      - Emergency response: stop server, revoke client cert, key compromise.
      - Certificate management & rotation.
      - Startup/shutdown, key management (`HSM_KEY_B64`).
      - Monitoring & audit log usage.
      - Troubleshooting (mTLS disabled, connection fails, fail-closed).
      - Recovery procedures + security checklist + quick reference.
- [x] 11. Enhance audit log with structured queryable fields.
      - `hsm_server` audit log now emits `timestamp`, `request_id`, `tx_hash`,
        and `status` ("signed" | "failed") per signing request.
      - Backward-compatible `ts_ms` / `signature_b64` fields retained.
      - CI `Check Audit Logs` step greps for `request_id`, `tx_hash`, `status`,
        `timestamp`, and verifies at least one `"status":"signed"` entry.
      - Runbook §6 updated with the new JSON schema + PowerShell query examples.
      - Unit test `sign_tx_invalid_returns_fallback_status` added (3/3 pass).
