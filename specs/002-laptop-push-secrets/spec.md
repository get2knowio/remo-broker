# Feature Specification: Laptop-Push Secrets Daemon

**Feature Branch**: `002-laptop-push-secrets`
**Created**: 2026-05-30
**Status**: Draft
**Supersedes**: [`001-broker-daemon`](../001-broker-daemon/) (external-backend / bootstrap-token model)
**Cross-repo dependency**: [`remo` spec 006](https://github.com/get2knowio/remo/tree/main/specs/006-credential-broker-laptop-push) (the laptop CLI half)

**Input**: Redesign the `remo-broker` daemon for a model where the developer's laptop pushes an encrypted, age-bundled set of project secrets to the instance at create time (and on subsequent `remo push-creds` calls), the daemon decrypts the bundle in memory using a systemd-credentials-sourced key (TPM2 → host-key → plaintext-mode-0600 fallback ladder), and serves cleartext secrets to devcontainers via the existing per-project Unix socket protocol. No external secret backend, no on-disk bootstrap token, no `fnox-core` dependency, no AWS-SM / Vault / 1Password integration.

## Why the redesign

`001-broker-daemon` was built around an external secret backend (1P / Vault / AWS-SM via `fnox-core`) and a per-instance bootstrap token on disk at `/etc/remo-broker/bootstrap-token` that the daemon used to fetch on demand. End-to-end testing on remo on 2026-05-29 surfaced that this design carries a residual on-disk credential (the bootstrap token) that contradicts the supply-chain threat model the broker was built to defend — and that the operational complexity of running a backend is unnecessary for the actual audience.

See [`remo:specs/006-credential-broker-laptop-push/spec.md`](https://github.com/get2knowio/remo/tree/main/specs/006-credential-broker-laptop-push) for the full motivation, threat model, and laptop-side requirements.

## What this changes in the daemon

The wire protocol bumps to **v2** per the additive-only-within-major rule in [`docs/wire-protocol.md` §4](../../docs/wire-protocol.md): removing the `rotate-bootstrap` admin op and the `bootstrap_mode` field from `StatusResponse` are breaking. A new artifact `schema/remo-broker.v2.json` ships alongside the v0.2.0 release.

## Requirements

### Functional

| ID | Requirement |
|---|---|
| FR-001 | The daemon reads its encrypted secrets bundle from `/var/lib/remo-broker/secrets.enc` (under `StateDirectory=remo-broker`, owned by the service user, mode 0600) at startup. |
| FR-002 | The decryption key is loaded via systemd's `LoadCredentialEncrypted=secrets-key:<source>`, exposed to the daemon at `$CREDENTIALS_DIRECTORY/secrets-key`. The daemon never reads the key from any other location. |
| FR-003 | The encryption primitive is `age` (X25519 + ChaCha20-Poly1305). The on-disk file is a standard age ciphertext encrypted to the instance's age public recipient. |
| FR-004 | The plaintext, once decrypted, is a TOML map of `{ secret_name = "value" }` (string values only; binary out of scope for v1). The map is held in memory as `Arc<ArcSwap<HashMap<String, SecretString>>>` (zeroize-on-drop via the existing `secrecy` crate). |
| FR-005 | If the encrypted bundle is absent at startup, the daemon binds its sockets and runs in a "no-secrets" mode; every `get` returns a `not_found` outcome. If the key is absent, the daemon refuses to start (hard error). |
| FR-006 | The admin socket exposes a new operation `push-creds` (NDJSON). Request: `{ "op": "push-creds", "ciphertext_b64": "<base64-age-ciphertext>" }`. Response: `{ "ok": true, "loaded_at": "<RFC3339>", "secret_count": N }` or an `ErrorResponse` with code `decrypt_failed` / `invalid_payload`. |
| FR-007 | `push-creds` writes the ciphertext to `secrets.enc.tmp`, calls `fsync`, then `rename`s atomically over `secrets.enc`. After the on-disk swap, the in-memory `ArcSwap` is replaced atomically. In-flight `get` requests complete against whichever snapshot they loaded. |
| FR-008 | The admin socket exposes a new operation `clear-creds`. Request: `{ "op": "clear-creds" }`. Response: `{ "ok": true }`. Effect: in-memory map is replaced with an empty map; `secrets.enc` is zeroized on disk (overwritten with zeros, then unlinked). |
| FR-009 | The admin socket exposes a new operation `get-public-key`. Request: `{ "op": "get-public-key" }`. Response: `{ "ok": true, "recipient": "age1..." }`. Effect: returns the instance's age public recipient so the laptop can encrypt to it. |
| FR-010 | The admin operation `rotate-bootstrap` (and its `BootstrapMode` companion type, and the `bootstrap_mode` field in `StatusResponse`) is removed. The `StatusResponse` gains `{ secrets_loaded_at: <Option<RFC3339>>, secret_count: <usize>, decryption_key_source: <"tpm2" | "host-key" | "plaintext"> }`. |
| FR-011 | A new audit event `AuditEvent::SecretsPushed { timestamp, secret_count, source: "push-creds" }` is emitted on successful `push-creds`. A new event `AuditEvent::SecretsCleared` on successful `clear-creds`. Values are not written; only counts. |
| FR-012 | The per-project socket protocol (`get` / `ping` / `info`), per-project manifest enforcement, per-project bounded cache, and audit log format from 001 carry forward unchanged except that `Outcome::BackendError` and `Outcome::BackendUnreachable` become unreachable in practice (kept in the enum to avoid wire-protocol churn for downstream parsers). |
| FR-013 | The daemon advertises `PROTOCOL_VERSION = 2` in admin `status` and project `ping` responses. |
| FR-014 | Wire schema `schema/remo-broker.v2.json` is generated by an extended `schema-gen` Cargo feature and published as a release artifact alongside the binaries. |

### Non-functional

| ID | Requirement |
|---|---|
| NFR-001 | Stripped Linux binary ≤ 15 MiB (the original 001 NFR target, missed at v0.1.0 due to `fnox-core` transitive deps including `hidapi`, `libudev`, AWS SDK, hyper/rustls/webpki). |
| NFR-002 | `Cross.toml` is deleted. `cargo build --target aarch64-unknown-linux-gnu` succeeds against the standard cross-rs image with no pre-build hooks. |
| NFR-003 | `deny.toml`'s `[advisories].ignore` list (currently 6 entries: RUSTSEC-2024-0375 atty, RUSTSEC-2023-0071 rsa Marvin, RUSTSEC-2025-0134 rustls-pemfile, RUSTSEC-2026-0098/-0099/-0104 webpki) is empty after the redesign; all 6 reach the broker via `fnox-core → AWS SDK`. |
| NFR-004 | `push-creds` admin op completes (decrypt + atomic-swap + audit) in < 50ms for a 10 KiB ciphertext, measured on a stock Debian 13 LXC. |
| NFR-005 | `get` request latency from a per-project socket connection is unchanged from 001 (sub-millisecond p99 against the in-memory store, since no backend roundtrip is involved). |
| NFR-006 | All other 001 NFRs (FR-023 systemd hardening profile, FR-022 graceful shutdown drain, FR-018 audit-log degraded buffer) carry forward unchanged. |

## What carries forward from 001 (the chassis)

Source files that stay essentially as-is (renames + import updates only):

- `src/proto/mod.rs` (NDJSON framing, 64 KiB cap, smoke-fuzz tests)
- `src/proto/project.rs` (`ProjectRequest::{Get, Ping, Info}`, `GetResponse`, `ProjectErrorCode`)
- `src/manifest.rs` (TOML parser, validators, manifest discovery, `MANIFEST_CANDIDATES`)
- `src/registry.rs` (`ProjectRegistry`, `Project`, per-project socket bind, atomic reload)
- `src/audit.rs` (NDJSON append-only writer, bounded channel + degraded buffer)
- `src/cache.rs` (`BoundedCache`, `SecretString`, zeroize semantics — kept for derived/decrypted values)
- `src/server.rs` core lifecycle (admin socket bind, accept loop, sigterm handling, `JoinSet` drain) — ~80% of the 1676 lines
- `src/config.rs` (after `BootstrapSource` / `BOOTSTRAP_ENV_VAR` / `backend_fetch_timeout` / `fnox_config_path` are removed)
- `packaging/systemd/remo-broker.service` (rename `LoadCredentialEncrypted=bootstrap-token` to `secrets-key`; everything else holds)
- `packaging/sysusers.d/remo-broker.conf` + `tmpfiles.d/remo-broker.conf` (verbatim)
- `schema/remo-broker.v1.json` (this is the **manifest** schema, not wire — unaffected)

## What gets ripped out

Source files deleted entirely:

- `src/backend.rs` (114 LOC) — only call site for `fnox_core::*`
- `src/bootstrap.rs` (819 LOC, including hand-rolled IMDSv2 HTTP/1.1 client + ~500 LOC of mock tests)

Code surgically excised from kept files:

- `src/main.rs`: `--bootstrap-source`, `--bootstrap-token-path`, `--fnox-config`, `--backend-fetch-timeout-ms` CLI flags; `fetch_token` startup validation; `BackendSession::open`/`discover` branch
- `src/config.rs`: `BootstrapSource` enum, `BootstrapSourceKind`, `BOOTSTRAP_ENV_VAR`, `DEFAULT_BOOTSTRAP_TOKEN_PATH`, `DEFAULT_BACKEND_FETCH_TIMEOUT_MS`, related `Overrides` / `RawConfig` fields, `ConfigError::BackendTimeoutZero`, 8 unit tests
- `src/server.rs`: `dispatch_rotate_bootstrap`, `bootstrap_mode()` helper, `AdminRequest::RotateBootstrap` arm, `backend: Option<BackendSession>` field + clone in fallback `Server`, `BackendSession` and `fetch_token` imports, 3 unit tests
- `src/proto/admin.rs`: `AdminRequest::RotateBootstrap`, `RotateBootstrapResponse`, `BackendAuthState`, `BootstrapMode`, `AdminErrorCode::BootstrapError`, `StatusResponse.bootstrap_mode`, 3 unit tests

Build / dependency artifacts:

- `Cross.toml` — delete entire file (libudev is gone with `fnox-core`)
- `Cargo.toml`: remove `fnox-core = "1.25"`; keep `secrecy` (still used by `cache.rs`); add `age` (~v0.10) for ciphertext handling
- `deny.toml` lines 10-34: delete the entire `[advisories].ignore` array
- `.github/workflows/release.yml` lines 49-58: drop the "Install native libudev (x86_64 fast path)" step; `cross` install can stay or be replaced with bare cargo + linker
- `.github/workflows/ci.yml` line 33-34: drop libudev install step

Examples / benches (logic carries; constructor changes):

- `examples/soak.rs`, `examples/killtest.rs`, `benches/latency.rs` — rewrite the harness to construct an `InMemorySecretStore` instead of a `BackendSession`

## What's new

- `src/store.rs` — new module. `InMemorySecretStore { inner: Arc<ArcSwap<HashMap<String, SecretString>>> }`. Constructed from a decrypted plaintext map; supports `get(name) -> Option<SecretString>` and `swap(new_map)`.
- `src/crypto.rs` — new module. age decrypt of `secrets.enc` ciphertext using the identity loaded from `$CREDENTIALS_DIRECTORY/secrets-key`. age encrypt is NOT needed in the daemon (only the laptop encrypts).
- Admin ops `push-creds`, `clear-creds`, `get-public-key` — new variants in `AdminRequest`, new response types, dispatch logic in `src/server.rs`.
- `AuditEvent::SecretsPushed` and `AuditEvent::SecretsCleared` — new variants in `src/audit.rs`.
- `schema/remo-broker.v2.json` — extended `schema-gen` feature emits the wire schema (currently only manifest is schema'd). The schema describes the v2 admin + project protocol.

## Cross-cutting decisions (mirrored from remo spec 006)

1. **`age` for encryption** — audited, multi-recipient native, mature Rust crate
2. **Decryption-key fallback ladder** — TPM2 → host-key → plaintext-mode-0600. The Ansible role on the remo side decides which tier; the daemon just reads whatever ends up at `$CREDENTIALS_DIRECTORY/secrets-key`. The chosen tier is surfaced in `StatusResponse.decryption_key_source` so operators can audit posture.
3. **Wire protocol v2** with published `schema/remo-broker.v2.json`
4. **No backward-compat shims with v0.1** — clean break; documentation flags the migration path for any existing user (which is essentially "wipe `/etc/remo-broker/`, install v0.2, re-push from laptop")

## Sequencing

| Day | Work |
|---|---|
| 1 | Delete `backend.rs`, `bootstrap.rs`, related config + tests. Verify `cargo build` clean without `fnox-core`. |
| 2 | Simplify `Cross.toml` (delete), `release.yml`, `ci.yml`, `deny.toml`. Verify cross-builds + `cargo deny check` green. |
| 3-4 | Implement `src/store.rs` + `src/crypto.rs` + systemd-credentials loading wiring. Unit tests for decrypt + atomic swap. |
| 5-6 | Implement `push-creds`, `clear-creds`, `get-public-key` admin ops + `AuditEvent::SecretsPushed/Cleared` variants. Update `dispatch_get` cache-miss path to hit `InMemorySecretStore`. Tests. |
| 7 | Rewrite `examples/soak.rs`, `examples/killtest.rs`, `benches/latency.rs` against new constructor. Update `StatusResponse` shape + downstream tests. Generate + commit `schema/remo-broker.v2.json`. |

Total: ~7 days focused work.

## What happens to 001 artifacts

- **`specs/001-broker-daemon/`** stays intact as historical reference. A "Status" header note is added marking it superseded by this spec.
- **`v0.1.0` release** stays published; `v0.2.0` will supersede on the remo side via `BROKER_PINNED_VERSION` bump.
- **`docs/wire-protocol.md`** rewritten as part of the implementation (removing `rotate-bootstrap` section, adding `push-creds` / `clear-creds` / `get-public-key`, documenting v2 schema).
- **`README.md`, `REMO_HANDOFF.md`, `docs/binary-size.md`, `CONTRIBUTING.md`** rewritten as part of the implementation.

## See also

- [remo spec 006](https://github.com/get2knowio/remo/tree/main/specs/006-credential-broker-laptop-push) — the laptop CLI half
- [001-broker-daemon spec](../001-broker-daemon/spec.md) — the superseded design
