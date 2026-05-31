# Feature Specification: In-Memory Secrets Daemon (Sidecar-Push Model)

**Feature Branch**: `002-laptop-push-secrets` *(branch name retained for PR continuity; the model has since been simplified — see [§Why the design pivoted twice](#why-the-design-pivoted-twice))*
**Created**: 2026-05-30 (laptop-push model)
**Pivoted**: 2026-05-31 (sidecar-push model)
**Status**: Draft
**Supersedes**: [`001-broker-daemon`](../001-broker-daemon/) (external-backend / bootstrap-token model)
**Cross-repo dependency**: [`remo` spec 006](https://github.com/get2knowio/remo/tree/main/specs/006-credential-broker-laptop-push) (the laptop + sidecar half)

**Input**: Redesign the `remo-broker` daemon as a purely in-memory secrets server. Project devcontainers fetch via the existing per-project Unix sockets with manifest allowlist enforcement (unchanged from v0.1.0). The secrets are pushed into the broker's memory by a *sidecar devcontainer* running on the same LXC instance, over a local Unix admin socket, as plaintext (no encryption-in-transit because there is no network in transit). The broker has no persistent storage of secrets, no external backend integration, no bootstrap token, no `fnox-core` dependency.

## Why the design pivoted twice

`001-broker-daemon` was built around an external secret backend (1P / Vault / AWS-SM via `fnox-core`) with a per-instance bootstrap token on disk. End-to-end testing of remo's 005 spec on 2026-05-29 surfaced that the bootstrap token at `/etc/remo-broker/bootstrap-token` is itself an on-disk credential, contradicting the supply-chain threat model.

The first 002 redesign (2026-05-30) replaced the external backend with a "laptop pushes age-encrypted blob to instance over SSH" model. Daemon would decrypt at startup from `/var/lib/remo-broker/secrets.enc` using a key sourced via systemd `LoadCredentialEncrypted=`. Cleaner — no backend, no bootstrap token — but still keyed on the laptop being the source-of-truth, with significant new daemon-side machinery (encrypted-blob reader, age decrypt, atomic file swap).

On 2026-05-31, the remo-side design pivoted again: the source-of-truth moves into a *sidecar devcontainer* on the same LXC instance. The sidecar holds the encrypted-at-rest fnox storage; it pushes plaintext to the broker over a local Unix socket; the broker is purely in-memory.

The cascading simplification on the daemon side is significant. **The encrypted-blob reader, age decrypt, atomic file swap, and `LoadCredentialEncrypted` for secrets all go away.** The daemon is now smaller than 001 was, despite gaining `push-creds` and `clear-creds` admin ops.

## What this changes in the daemon

The wire protocol bumps to **v2** per the additive-only-within-major rule in [`docs/wire-protocol.md` §4](../../docs/wire-protocol.md): removing the `rotate-bootstrap` admin op and the `bootstrap_mode` field from `StatusResponse` are breaking. A new artifact `schema/remo-broker.v2.json` ships alongside the v0.2.0 release.

## Requirements

### Functional

| ID | Requirement |
|---|---|
| FR-001 | The daemon starts with an empty in-memory secrets store. There is no on-disk secrets blob to read at startup. |
| FR-002 | The daemon does not require any secrets-related credential from systemd-credentials at startup. The systemd unit's `LoadCredentialEncrypted=` directive (used in v0.1.0 for the bootstrap token) is removed. |
| FR-003 | The admin socket exposes a new operation `push-creds` (NDJSON). Request: `{ "op": "push-creds", "secrets": { "<name>": "<value>", ... } }`. The map is plaintext — no encryption envelope. Response: `{ "ok": true, "loaded_at": "<RFC3339>", "secret_count": N }` or `ErrorResponse` with code `invalid_payload` / `payload_too_large`. |
| FR-004 | `push-creds` atomically replaces the in-memory secrets map via `ArcSwap`. In-flight `get` requests complete against whichever snapshot they loaded. |
| FR-005 | The admin socket exposes a new operation `clear-creds`. Request: `{ "op": "clear-creds" }`. Response: `{ "ok": true }`. Effect: in-memory map is replaced with an empty map. |
| FR-006 | The admin operation `rotate-bootstrap` (and its `BootstrapMode` companion type, and the `bootstrap_mode` field in `StatusResponse`) is removed. The `StatusResponse` gains `{ secrets_loaded_at: <Option<RFC3339>>, secret_count: <usize> }`. The `decryption_key_source` field considered in the first 002 draft is removed (no key is sourced in the daemon now). |
| FR-007 | A new audit event `AuditEvent::SecretsPushed { timestamp, secret_count }` is emitted on successful `push-creds`. A new event `AuditEvent::SecretsCleared { timestamp }` on successful `clear-creds`. Values are not written; only counts. |
| FR-008 | The per-project socket protocol (`get` / `ping` / `info`), per-project manifest enforcement, per-project bounded cache, and audit log format from 001 carry forward unchanged except that `Outcome::BackendError` and `Outcome::BackendUnreachable` become unreachable in practice (kept in the enum to avoid wire-protocol churn for downstream parsers). |
| FR-009 | The daemon advertises `PROTOCOL_VERSION = 2` in admin `status` and project `ping` responses. |
| FR-010 | Wire schema `schema/remo-broker.v2.json` is generated by an extended `schema-gen` Cargo feature and published as a release artifact alongside the binaries. |
| FR-011 | The admin socket's UNIX permissions (mode 0660, root-owned, group-accessible) must allow the sidecar devcontainer's bind-mount user to call admin ops. The remo Ansible role handles group setup; the daemon itself just binds the socket with the configured mode. |
| FR-012 | Push payload size limit: the daemon accepts `push-creds` requests up to 1 MiB (raised from the v0.1.0 `MAX_MESSAGE_BYTES = 64 KiB`, since a realistic credential bundle can be larger than a single admin request). |

### Non-functional

| ID | Requirement |
|---|---|
| NFR-001 | Stripped Linux binary ≤ 15 MiB (the original 001 NFR target, missed at v0.1.0 due to `fnox-core` transitive deps). |
| NFR-002 | `Cross.toml` is deleted. `cargo build --target aarch64-unknown-linux-gnu` succeeds against the standard cross-rs image with no pre-build hooks. |
| NFR-003 | `deny.toml`'s `[advisories].ignore` list (currently 6 entries reaching via `fnox-core → AWS SDK`) is empty after the redesign. |
| NFR-004 | `push-creds` admin op completes (validate + atomic-swap + audit) in < 20 ms for a typical 10-secret payload, measured on a stock Debian LXC. |
| NFR-005 | `get` request latency from a per-project socket is unchanged from 001 (sub-millisecond p99 against the in-memory store; no backend roundtrip). |
| NFR-006 | All other 001 NFRs (FR-023 systemd hardening profile, FR-022 graceful shutdown drain, FR-018 audit-log degraded buffer) carry forward unchanged. |

## What carries forward from 001 (the chassis)

Source files that stay essentially as-is (renames + import updates only):

- `src/proto/mod.rs` — NDJSON framing, smoke-fuzz tests. `MAX_MESSAGE_BYTES` raised from 64 KiB to 1 MiB (or a per-op cap with `push-creds` higher than others).
- `src/proto/project.rs` — `ProjectRequest::{Get, Ping, Info}`, `GetResponse`, `ProjectErrorCode`
- `src/manifest.rs` — TOML parser, validators, manifest discovery. The remo side may extend the manifest schema with `fetch_as` per-secret directives; the broker does not interpret those (they're for the project devcontainer's `remo-fetch-secrets` helper) but the schema generator will need to know about them.
- `src/registry.rs` — `ProjectRegistry`, `Project`, per-project socket bind, atomic reload
- `src/audit.rs` — NDJSON append-only writer, bounded channel + degraded buffer (extended with new event variants)
- `src/cache.rs` — `BoundedCache`, `SecretString`, zeroize semantics (kept for the per-project cache between `get` calls)
- `src/server.rs` core lifecycle (~80% of the 1676 lines)
- `src/config.rs` (after the bootstrap-related fields are removed)
- `packaging/systemd/remo-broker.service` — **remove** the `LoadCredentialEncrypted=` block entirely; the daemon doesn't load any secret from systemd at startup. Keep the rest of the FR-023 hardening profile.
- `packaging/sysusers.d/remo-broker.conf` + `tmpfiles.d/remo-broker.conf` — verbatim
- `schema/remo-broker.v1.json` — this is the **manifest** schema; carries forward with `fetch_as` extension

## What gets ripped out

Source files deleted entirely:

- `src/backend.rs` (114 LOC) — only call site for `fnox_core::*`
- `src/bootstrap.rs` (819 LOC, including hand-rolled IMDSv2 HTTP/1.1 client + ~500 LOC of mock tests)

Code surgically excised from kept files:

- `src/main.rs`: all bootstrap-related CLI flags; `fetch_token` startup validation; `BackendSession::open`/`discover` branch
- `src/config.rs`: `BootstrapSource` enum, related fields, ~8 unit tests
- `src/server.rs`: `dispatch_rotate_bootstrap`, `bootstrap_mode()`, `AdminRequest::RotateBootstrap` arm, `backend: Option<BackendSession>` field, related imports, 3 unit tests
- `src/proto/admin.rs`: `RotateBootstrap` request/response/error variants, `BackendAuthState`, `BootstrapMode`, `StatusResponse.bootstrap_mode`, related tests

Build / dependency artifacts:

- `Cross.toml` — delete entire file (libudev is gone with `fnox-core`)
- `Cargo.toml`: remove `fnox-core = "1.25"`; remove `age` / `pyrage` / similar (none were added — but the first 002 draft would have added `age`; this rewrite removes that addition)
- `deny.toml` lines 10-34: delete the entire `[advisories].ignore` array
- `.github/workflows/release.yml`: drop the libudev fast-path step; `cross` install can stay or be replaced with bare cargo + linker
- `.github/workflows/ci.yml`: drop libudev install step

Compared to the first 002 draft (laptop-push with age encryption), additionally NOT building:

- `src/store.rs` as designed with encrypted-blob reader — instead a much simpler `InMemorySecretStore { inner: ArcSwap<HashMap<...>> }` with no decrypt logic
- `src/crypto.rs` for age decrypt — not needed
- Atomic write-to-tmp-then-rename for the on-disk blob — no on-disk blob
- `LoadCredentialEncrypted=secrets-key` in the systemd unit — removed entirely
- `get-public-key` admin op — not needed (no encryption, no pubkey)
- The on-disk-blob fsync + rename + verify dance — not needed

Examples / benches (logic carries; constructor changes):

- `examples/soak.rs`, `examples/killtest.rs`, `benches/latency.rs` — rewrite the harness to construct a daemon with an empty `InMemorySecretStore` and to call `push-creds` admin op as part of the workload

## What's new

Smaller surface than the first 002 draft:

- `src/store.rs` — new module. `InMemorySecretStore { inner: Arc<ArcSwap<HashMap<String, SecretString>>> }`. `get(name) -> Option<SecretString>`, `swap(new_map)`. That's it. No file I/O, no crypto.
- Admin ops `push-creds`, `clear-creds` — new variants in `AdminRequest`, new response types, dispatch logic in `src/server.rs`
- `AuditEvent::SecretsPushed` and `AuditEvent::SecretsCleared` — new variants in `src/audit.rs`
- `schema/remo-broker.v2.json` — extended `schema-gen` feature emits the v2 wire schema

## Cross-cutting decisions

1. **No encryption in the daemon at all.** Push is plaintext over local Unix socket; in-memory store is plaintext. The sidecar handles encryption-at-rest on its own side (with its own fnox storage); the broker is downstream of that boundary and doesn't need to know.
2. **No `LoadCredentialEncrypted` in the systemd unit.** Daemon doesn't load any credential from systemd at startup. (The remo Ansible side still uses `systemd-creds` to encrypt the sidecar's fnox-storage decryption key on the LXC host — but that's a sidecar concern, not a broker concern.)
3. **Wire protocol v2** with published `schema/remo-broker.v2.json`
4. **`MAX_MESSAGE_BYTES` raised to 1 MiB** for `push-creds` specifically. Other ops keep their existing limits. A per-op cap is cleaner than a global one; implementer's call.
5. **No backward-compat shims with v0.1.** Clean break; documentation flags the migration path.

## Sequencing

| Day | Work |
|---|---|
| 1 | Delete `backend.rs`, `bootstrap.rs`, related config + tests. Verify `cargo build` clean without `fnox-core`. |
| 2 | Simplify `Cross.toml` (delete), `release.yml`, `ci.yml`, `deny.toml`. Verify cross-builds + `cargo deny check` green. |
| 3 | Implement `src/store.rs` (~50 LOC). Update `dispatch_get` to use it. Update systemd unit (remove `LoadCredentialEncrypted=`). |
| 4 | Implement `push-creds` + `clear-creds` admin ops + `AuditEvent::SecretsPushed/Cleared` variants. Update `StatusResponse` shape. Tests. |
| 5 | Raise message-size cap (1 MiB for push-creds). Rewrite `examples/soak.rs`, `killtest.rs`, `benches/latency.rs`. Update wire-protocol doc + generate `schema/remo-broker.v2.json`. |

**Total**: ~5 days focused work. (Down from ~7 in the first 002 draft.)

## What happens to 001 artifacts

- **`specs/001-broker-daemon/`** stays intact as historical reference; status header marked superseded by this spec.
- **`v0.1.0` release** stays published; `v0.2.0` will supersede on the remo side via `BROKER_PINNED_VERSION` bump.
- **`docs/wire-protocol.md`** rewritten as part of the implementation (remove `rotate-bootstrap` section, add `push-creds` / `clear-creds`, document v2 schema).
- **`README.md`, `REMO_HANDOFF.md`, `docs/binary-size.md`, `CONTRIBUTING.md`** rewritten as part of the implementation.

## What happens to the first 002 draft

- The first 002 draft (2026-05-30, laptop-push with age encryption) is captured in this PR's commit history (commit `4db63ea`) for archival reference
- This rewrite (2026-05-31, sidecar-push) supersedes it within the same spec dir; no separate spec number used

## See also

- [remo spec 006](https://github.com/get2knowio/remo/tree/main/specs/006-credential-broker-laptop-push) — the laptop + sidecar + project-devcontainer half
- [001-broker-daemon spec](../001-broker-daemon/spec.md) — the superseded design
