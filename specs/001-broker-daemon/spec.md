# Feature Specification: remo-broker Daemon

**Feature Branch**: `001-broker-daemon`
**Created**: 2026-05-24
**Status**: Draft
**Input**: User description: "A long-lived Rust daemon for Linux instances that holds a per-instance bootstrap token, authenticates upward to a credential backend (1Password / Vault / AWS Secrets Manager / age / OS keychain via the fnox-core library), and serves per-project Unix sockets enforcing per-project allowlists. Built to be the on-instance half of Remo's credential-broker feature (see Remo `005-credential-broker/spec.md`)."

## Background and Motivation

The Remo credential-broker feature (spec'd in `get2knowio/remo` at `specs/005-credential-broker/spec.md`) defends against supply-chain attacks by removing long-lived developer credentials from Remo instances. The on-instance half of that design — the **broker daemon** — is the subject of this spec.

The Remo spec captures the *integration contract*: what the laptop CLI installs, what the developer experiences in the project menu, how the manifest is synthesized, what guarantees the feature provides end-to-end. This spec captures the *daemon internals*: what the broker does as a standalone piece of software, how it's structured, what its operational behavior is.

Why a separate spec:

- The daemon lives in a separate repo (different language, different release cadence, different audit surface).
- The Remo spec is silent on internal architecture by design — it specifies *what Remo needs from the broker*, not *how the broker works internally*.
- A clear daemon-internal spec lets security reviewers, contributors, and future maintainers reason about the broker without needing the Remo-product context.

## Terms and Definitions

These terms supplement the Remo spec's terminology and apply to broker-internal discussion.

| Term | Definition |
|---|---|
| **Daemon** | The `remo-broker` process. Runs as a systemd unit (`remo-broker.service`). One per instance. |
| **Bootstrap source** | The mechanism by which the daemon obtains its long-lived backend identity at startup. One of: file path (`/etc/remo-broker/bootstrap-token`), IMDSv2 (AWS instance profile), environment variable (development only). |
| **Backend session** | An authenticated handle to the upstream credential store, held by fnox-core internally. Re-established on `rotate-bootstrap` or after backend auth expiry. |
| **Project** | A registered project with an associated allowlist and per-project socket. Created via `register` on the admin socket; torn down via `unregister`. |
| **Project socket** | Per-project Unix domain socket at `/run/remo-broker/<name>.sock`, mode 0660. See `docs/wire-protocol.md`. |
| **Admin socket** | Single Unix domain socket at `/run/remo-broker/admin.sock`, mode 0600 root-only. See `docs/wire-protocol.md`. |
| **Cache entry** | An in-memory mapping from `(project, secret_name)` to `(SecretBox, fetched_at, ttl)`. `SecretBox` is fnox-core's secret-holding type (zeroized on drop). |
| **Audit event** | A single line in the audit log file. JSONL format. Records project, secret name, allow/deny decision, outcome, and timing. Never records values. |
| **Manifest** | The parsed-and-validated `remo-broker.toml` for a project. See `docs/manifest-schema.md`. |
| **Protocol version** | The wire-protocol major version the daemon speaks. Currently `1`. |

## User Scenarios & Testing *(mandatory)*

User stories here are scoped to the daemon's audience: the consumers of its sockets (in-devcontainer tools, Remo's broker manager) and its operator (the sysadmin or automated process that runs it on the instance).

### User Story 1 — Devcontainer fetches an allowed secret (Priority: P1)

A process inside a devcontainer connects to the bind-mounted project socket and requests a secret name that the project's manifest permits. The daemon returns the value within tens of milliseconds, satisfying the request from cache when warm and from the backend when cold. The value never appears in the daemon's logs or on disk.

**Why this priority**: This is the daemon's primary purpose. If this doesn't work, nothing else matters.

**Independent Test**: Register a project with a manifest declaring `TEST_SECRET`. Backend-side, ensure `TEST_SECRET=hello` is resolvable. From the project socket, send `{"op":"get","name":"TEST_SECRET"}`. Expect `{"ok":true,"value":"hello","ttl_seconds":N}` within 500ms cold, within 50ms warm. Verify no `hello` substring in the daemon log file or in `/proc/<pid>/maps`-derived heap dumps after the cache TTL expires.

**Acceptance Scenarios**:

1. **Given** a registered project with allowlist `["FOO"]` and backend value `FOO=bar`, **When** the project socket receives `get FOO`, **Then** response is `{"ok":true,"value":"bar",...}` within 50ms (warm cache).
2. **Given** the same project, **When** the project socket receives `get BAZ`, **Then** response is `{"ok":false,"code":"denied",...}` and an audit event with `decision=deny, reason=allowlist` is written.
3. **Given** a project with allowlist `["FOO"]`, **When** the backend is unreachable and no cached value exists for `FOO`, **Then** response is `{"ok":false,"code":"backend_unreachable",...}` and an audit event with `outcome=backend_unreachable` is written.
4. **Given** a cached value for `FOO` with 5s remaining TTL, **When** the backend is unreachable and a `get FOO` arrives, **Then** the cached value is returned with the actual remaining TTL.

---

### User Story 2 — Daemon survives restarts and backend outages (Priority: P1)

The daemon can be restarted (intentional upgrade or `systemctl restart`) and resume serving without manual reconfiguration. During a backend network outage, in-flight fetches that hit the cache succeed; fetches for uncached or expired secrets fail clearly rather than blocking indefinitely or returning stale values past their TTL.

**Why this priority**: A broker that requires hand-holding to recover from restarts or that hangs under backend outage breaks the dev environment in subtle ways. Operational correctness is table-stakes.

**Independent Test**: Start the daemon, register a project, fetch a secret successfully. `systemctl restart remo-broker`; verify the project socket reappears within 2s of restart and serves fetches correctly. Separately, simulate a backend outage (block egress to backend IPs); verify cached fetches succeed and uncached fetches return `backend_unreachable` within the configured timeout.

**Acceptance Scenarios**:

1. **Given** the daemon is running with N registered projects, **When** `systemctl restart remo-broker` runs, **Then** all N project sockets exist and accept connections within 2 seconds, all caches are cold (acceptable: cache is in-memory only by design).
2. **Given** a backend network outage, **When** the daemon receives a `get` for a cached secret, **Then** the cached value is returned.
3. **Given** the same outage, **When** the daemon receives a `get` for an uncached secret, **Then** the response is `{"ok":false,"code":"backend_unreachable",...}` within 5 seconds (not blocking indefinitely).
4. **Given** a cached secret whose TTL has expired during an outage, **When** a `get` for it arrives, **Then** the response is `{"ok":false,"code":"backend_unreachable",...}` — the daemon does NOT serve expired values.

---

### User Story 3 — Project lifecycle (Priority: P1)

Remo (or another admin) registers a project via the admin socket; the daemon creates the project socket. The manifest can be reloaded without socket teardown. Unregistering destroys the socket and forgets the project's cached values.

**Why this priority**: Project lifecycle is the daemon's control surface. Manifests change during normal development; projects come and go as developers create and destroy devcontainers. The daemon must handle these correctly.

**Independent Test**: Send `register` with a valid manifest path; verify socket appears. Edit the manifest to add a new allowed name; send `reload`; verify the new name resolves. Send `unregister`; verify the socket disappears and subsequent `get` attempts (via a held connection from before unregister) error cleanly.

**Acceptance Scenarios**:

1. **Given** a valid manifest at `/projects/foo/.devcontainer/remo-broker.toml`, **When** admin sends `register foo /projects/foo`, **Then** `/run/remo-broker/foo.sock` exists with mode 0660, and `get`s on it succeed for allowed names.
2. **Given** a registered project, **When** the manifest is edited and admin sends `reload foo`, **Then** the new allowlist is in effect for subsequent fetches; in-flight connections see the new allowlist on their next request (no connection drop).
3. **Given** a registered project, **When** admin sends `unregister foo`, **Then** the socket file is removed within 100ms and the project's exclusive cache entries are dropped from memory.
4. **Given** a manifest that fails validation, **When** admin sends `register`, **Then** the response is `{"ok":false,"code":"manifest_invalid","message":"..."}` and no socket is created.

---

### User Story 4 — Audit log captures every fetch decision (Priority: P2)

Every secret-fetch attempt — allowed or denied, succeeding or failing — is recorded in an append-only audit log. The log can be tailed by an operator to investigate suspicious activity. The log never contains secret values.

**Why this priority**: Without audit logging, a successful supply-chain attack might be invisible. With it, "did the malicious npm install try to fetch NPM_TOKEN?" is answerable.

**Independent Test**: Perform a sequence of fetches: 1 allowed, 1 denied, 1 backend_error. Read the audit log; verify each appears as a separate JSONL line with the expected fields. Grep the log for any of the secret values; expect zero matches.

**Acceptance Scenarios**:

1. **Given** any fetch occurs, **When** the daemon writes the audit event, **Then** the event includes: timestamp (RFC3339 UTC), project name, secret name, decision (`allow`/`deny`), outcome (`ok`/`backend_unreachable`/`not_found`/`backend_error`), peer PID, peer UID, request latency (ms).
2. **Given** the daemon is running, **When** the audit log file is moved (rotation), **Then** the daemon opens a new file on the next event (no SIGHUP required if using `O_APPEND` + open-per-write; otherwise SIGHUP triggers reopen).
3. **Given** any audit event, **When** an operator greps the log for known secret values, **Then** zero matches are found — the log never contains values.

---

### User Story 5 — Bootstrap token rotation (Priority: P2)

The admin can trigger the daemon to re-read its bootstrap token (e.g., after Remo has minted a fresh one) without restarting the daemon. The cache survives the rotation; fetches in flight at rotation time are not disrupted.

**Why this priority**: Rotation is the lifecycle mechanism that bounds bootstrap-token exposure. Without an in-place rotation operation, every rotation requires a daemon restart, which loses cache and drops connections — making rotation operationally expensive and therefore likely to be done less often than it should be.

**Independent Test**: Register a project, perform several fetches to populate cache. Replace the bootstrap token file on disk with a fresh token; send `rotate-bootstrap` to the admin socket; verify the response is `{"ok":true,"backend_auth":"ok"}`. Perform another fetch for a different (uncached) secret; verify it succeeds (proves the new token is in use). Verify cache entries from before rotation are still valid.

**Acceptance Scenarios**:

1. **Given** a rotated bootstrap token on disk, **When** admin sends `rotate-bootstrap`, **Then** the daemon re-reads the token, re-authenticates to the backend, and the response confirms `backend_auth: ok`.
2. **Given** rotation succeeds, **When** subsequent fetches occur, **Then** they use the new backend session; cached values from before the rotation remain usable until their TTL.
3. **Given** rotation fails (new token invalid, backend rejects auth), **When** admin sends `rotate-bootstrap`, **Then** the daemon retains the previous backend session, returns `{"ok":false,"code":"bootstrap_error",...}`, and continues serving from cache.

---

### User Story 6 — Multi-backend retrieval via fnox-core (Priority: P2)

The daemon does not know the specifics of any backend (1Password, Vault, AWS Secrets Manager, age, keychain). It hands secret-name resolution to fnox-core, which selects the configured backend per-name and authenticates using whatever the bootstrap source provides.

**Why this priority**: This is the architectural lever that lets one binary serve developers across different secret-store choices without per-store branching in the broker. If we re-implemented backend integrations inline, the broker would carry the maintenance weight of every backend's SDK and auth quirks.

**Independent Test**: Configure fnox-core (on the instance) to route different secret names to different backends — `GITHUB_TOKEN` to 1Password, `NPM_TOKEN` to Vault, `ANTHROPIC_API_KEY` to keychain. Register a project allowing all three. Verify all three resolve through the same project socket without the broker code containing any backend-specific logic.

**Acceptance Scenarios**:

1. **Given** fnox-core configured with three backends mapping three different names, **When** the broker resolves each, **Then** all three succeed transparently and audit events record `backend = <name>` per fnox-core's identification.
2. **Given** a backend that requires interactive auth (e.g., 1Password biometric) and an instance with no interactive context, **When** a fetch routed to that backend occurs, **Then** the broker returns `backend_error` with a message identifying the interactive-auth requirement — per the Remo spec OQ-5, instance use should be limited to non-interactive backend identities.

---

### Edge Cases

- **Two registrations with the same name**: second `register` returns `project_exists`. No silent replacement.
- **Manifest renamed/deleted at runtime**: `reload` returns `manifest_not_found`. Existing socket continues serving the previously-loaded allowlist until `unregister` or daemon restart.
- **Project socket file deleted out-of-band**: the daemon detects the loss via the next accept loop error and re-creates the socket (logged as `socket.recovered`).
- **Audit log filesystem full**: writes fail; the daemon serves fetches with a degraded-mode audit event in memory (last 1000 events) and logs a critical error. Fetches MUST NOT be blocked by audit-write failures (audit is a soft requirement; serving is hard).
- **Bootstrap token file removed at runtime**: daemon continues with the in-memory backend session; next `rotate-bootstrap` fails with `bootstrap_error`.
- **Devcontainer restarts repeatedly**: each restart opens a new socket connection; the daemon does not maintain devcontainer-process identity across reconnects. PIDs are recorded in audit events as best-effort.
- **systemd stops the daemon mid-fetch**: in-flight backend RPCs are cancelled via tokio task cancellation; the daemon writes a shutdown audit event before exit.

## Requirements *(mandatory)*

### Functional Requirements

**Configuration and bootstrap**

- **FR-001**: The daemon MUST load configuration from `/etc/remo-broker/config.toml` if present, with CLI flag overrides. Document keys: `bootstrap_source` (`file` / `imds` / `env`), `bootstrap_token_path`, `socket_dir`, `audit_log_path`, `cache_default_ttl_seconds`, `cache_default_max_entries`, `backend_fetch_timeout_ms`.
- **FR-002**: The daemon MUST support three bootstrap sources, selected by configuration: (a) `file` — read from a path, default `/etc/remo-broker/bootstrap-token`; (b) `imds` — fetch from AWS IMDSv2; (c) `env` — read from `REMO_BROKER_BOOTSTRAP_TOKEN` (development/testing only, warned at startup).
- **FR-003**: The daemon MUST exit with non-zero status and a clear stderr message if no bootstrap source yields a usable token at startup.

**fnox-core integration**

- **FR-004**: The daemon MUST use `fnox-core` (Cargo dependency) for all backend retrieval. The daemon code MUST NOT contain backend-specific (1Password / Vault / AWS / age / keychain) logic.
- **FR-005**: The daemon MUST construct a single `Fnox` instance at startup (or post-`rotate-bootstrap`), share it across all project handlers, and reuse backend connections.

**Socket lifecycle**

- **FR-006**: At startup, the daemon MUST create `/run/remo-broker/` if missing (mode 0755, owner `remo-broker:remo-broker`), then create the admin socket at `/run/remo-broker/admin.sock` (mode 0600, owner `root:root`).
- **FR-007**: On admin `register`, the daemon MUST create the project socket at `/run/remo-broker/<name>.sock` (mode 0660, owner `remo-broker:remo-broker`, additional group access configured per the project's devcontainer ownership).
- **FR-008**: On admin `unregister` or daemon shutdown, the daemon MUST remove project sockets and the admin socket from the filesystem.
- **FR-009**: The daemon MUST handle the case where a socket file persists from a previous run (stale socket) by removing it before binding.

**Manifest handling**

- **FR-010**: The daemon MUST parse and validate manifests per `docs/manifest-schema.md`. Validation failures cause `register`/`reload` to fail without affecting any other project.
- **FR-011**: The daemon MUST atomically swap the in-memory allowlist on `reload` so that no fetch sees a partially-updated allowlist.

**Allowlist enforcement**

- **FR-012**: The daemon MUST check every `get` on a project socket against that project's allowlist *before* invoking fnox-core. Denied fetches MUST NOT incur a backend round-trip.
- **FR-013**: The daemon MUST record an audit event for every fetch attempt, including denials. Audit-event format per `docs/wire-protocol.md` and the Audit Log section below.

**Caching**

- **FR-014**: The daemon MUST cache successful backend retrievals in memory with a TTL (default 900s, per-project cap via manifest). Cache keys are `(project, secret_name)` — a value cached for one project is NOT shared with another even if the name matches.
- **FR-015**: The daemon MUST NOT persist cached values to disk under any condition.
- **FR-016**: The daemon MUST zeroize cached value memory on eviction and on `unregister`. Implementation uses fnox-core's `SecretBox` or equivalent.

**Audit log**

- **FR-017**: The daemon MUST write one JSONL audit event per fetch attempt to `/var/log/remo-broker/audit.log` (path configurable). Events MUST contain: timestamp, project, secret_name, decision, outcome, peer_pid, peer_uid, latency_ms, backend (when applicable). Events MUST NOT contain values.
- **FR-018**: Audit-log file failures MUST NOT block serving — see edge cases.

**Wire protocol**

- **FR-019**: The daemon MUST implement the project-socket and admin-socket protocols per `docs/wire-protocol.md` exactly, including all listed error codes.
- **FR-020**: The daemon MUST advertise its `broker_version` and `protocol_version` in `ping` and `status` responses.

**systemd integration**

- **FR-021**: The daemon MUST send `READY=1` to systemd via `sd_notify` after sockets are bound and the backend session is established at startup.
- **FR-022**: The daemon MUST handle SIGTERM by closing listening sockets (refusing new connections), allowing in-flight requests up to 5 seconds to complete, then exiting cleanly.
- **FR-023**: The shipped systemd unit MUST set `LimitCORE=0`, `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, `MemoryDenyWriteExecute=yes`, `RestrictSUIDSGID=yes`, `User=remo-broker`, `Group=remo-broker`, `ReadWritePaths=/run/remo-broker /var/log/remo-broker`, and `LoadCredentialEncrypted=bootstrap-token:/etc/remo-broker/bootstrap-token` where TPM2 sealing is available.

**Concurrency**

- **FR-024**: The daemon MUST handle multiple concurrent project-socket connections from one or many projects without serializing fetches behind a global lock.

### Non-Functional Requirements

- **NFR-001**: Warm-cache `get` latency MUST be ≤5 ms p99 (measured at the socket boundary, single connection, idle system).
- **NFR-002**: Cold (cache-miss) `get` latency is bounded by the backend's response time + ≤20 ms of broker overhead.
- **NFR-003**: Startup time from process exec to `READY=1` MUST be ≤500 ms on a typical instance (excluding backend round-trip for initial auth verification, which is dominated by network latency).
- **NFR-004**: Idle memory footprint (no cached entries, one registered project) MUST be ≤30 MB RSS.
- **NFR-005**: The daemon binary MUST be statically linked (where the OS permits — Linux musl target) or use only platform-default dynamic libraries (glibc, libc), with a stripped release-build size ≤15 MB.
- **NFR-006**: The daemon MUST compile against the latest stable Rust toolchain and the toolchain identified in `rust-version` in `Cargo.toml`; older toolchains are not supported.
- **NFR-007**: The daemon MUST pass `cargo audit`, `cargo deny check`, and `cargo clippy --all-targets -- -D warnings` clean in CI.

### Key Entities

- **Manifest** (struct): parsed `remo-broker.toml`; fields per `docs/manifest-schema.md`. Validated at parse time; never mutated.
- **Project** (struct): `name`, `manifest: Arc<Manifest>`, `socket_listener: tokio::net::UnixListener`, `cache: BoundedCache<SecretName, CacheEntry>`. Stored in a `RwLock<HashMap<String, Project>>` keyed by name.
- **CacheEntry** (struct): `value: SecretBox`, `fetched_at: Instant`, `ttl: Duration`. `value` zeroized on drop.
- **AuditEvent** (struct): the JSONL audit event; implements `Serialize`.
- **BootstrapSource** (enum): `File(PathBuf)` / `Imds` / `Env(String)`. Used at startup and on `rotate-bootstrap`.
- **BackendSession** (held inside fnox-core): an authenticated `Fnox` instance. Swappable atomically via `ArcSwap` on rotation.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A fuzz test against the project-socket NDJSON parser (operating on random byte sequences) MUST run 24 hours with zero panics and zero memory growth.
- **SC-002**: A 1-hour soak test with 50 simulated devcontainers performing mixed `get`/`ping`/`info` at 10 Hz each MUST exhibit no memory growth beyond the cache's configured maximum and no missed audit events.
- **SC-003**: A killtest (SIGKILL repeatedly applied during random points in fetch handling) MUST yield no on-disk artifacts containing secret values and no broken project sockets after systemd restart.
- **SC-004**: Grep of the audit log and the daemon's stdout/stderr after a full integration test MUST find zero substrings matching any of the test secret values.
- **SC-005**: A red-team exercise — a hostile process inside a devcontainer attempting to fetch secrets outside the allowlist, brute-force secret names, exhaust the cache, or escalate to other projects' sockets — MUST recover only the values declared in the manifest, and the audit log MUST record every attempt.
- **SC-006**: End-to-end CI test (cross-repo, against the Remo Python codebase) MUST pass: Remo synthesizes a manifest → admin registers → devcontainer-side tool fetches an allowed secret → outcome verified → unregister → socket removed.

## Out of Scope

- **Backend SDK details, auth flows, and secret-engine specifics**: handled by fnox-core. The broker depends on the library's interface and does not reimplement any of it.
- **Manifest synthesis by Remo**: how Remo decides to create a manifest, what defaults it picks, and what it writes to `.remo/broker.toml` is Remo's concern, specified in the Remo repo's spec.
- **Bootstrap-token minting and rotation policy**: minting happens on the laptop or a node helper; the broker only consumes a token. Rotation scheduling is Remo's concern; the broker only provides the in-place `rotate-bootstrap` operation.
- **Multi-user instances**: each instance is single-developer. Multi-tenant scoping is deferred (would require per-user project segregation, per-user audit-log views, and likely per-user backend identities).
- **Devcontainer attestation beyond the bind-mount**: the broker does not verify the binary identity of callers. The trust boundary is the devcontainer; anything in it has the project's allowlist.
- **TPM2 sealing implementation**: handled by systemd's `LoadCredentialEncrypted` and `LoadCredentialEncrypted` units, not by broker code. Broker reads the resulting plaintext token from `$CREDENTIALS_DIRECTORY/bootstrap-token` at startup.
- **Telemetry / Prometheus**: not in v1. May be added as an optional feature behind a Cargo `features` flag.
- **Cross-platform support**: Linux only. macOS and Windows are not supported targets; the daemon uses Linux-specific primitives (`SO_PEERCRED`, systemd sockets, IMDSv2).

## Open Questions

- **OQ-1**: Should the broker support hot-reloading its own `/etc/remo-broker/config.toml`, or is a `systemctl restart` the right way to apply config changes? Hot reload is more user-friendly but adds a state machine complexity that may not be worth it for a config that changes rarely.
- **OQ-2**: For the IMDS bootstrap source, should the broker periodically refresh the credentials on its own (since IMDS-derived credentials have short TTLs), or rely on fnox-core to handle that internally? Likely the latter, but needs confirmation against fnox-core's actual behavior.
- **OQ-3**: Should the cache be LRU (max_entries-bounded) or strictly TTL-bounded (no max)? Bounded gives predictable memory; unbounded matches the spec's "in-memory only" guarantee more cleanly but allows DoS via cache flooding.
- **OQ-4**: Should the project socket support an optional `prefetch` operation that takes a list of names and warms the cache for all of them, useful for devcontainer startup latency? Adds protocol surface; can be added in a minor version.
- **OQ-5**: Should the daemon ship a separate `remo-broker-admin` CLI that wraps the admin socket protocol, or should admin operations always go through `nc -U` / Remo's own client? A small CLI improves operator UX (e.g., `remo-broker-admin status` vs. constructing JSON by hand).
- **OQ-6**: For `peer_unexpected` — exactly which UIDs are "expected" for a given project socket? Naive answer: the devcontainer's effective UID. Implementation: configurable per-project, or derived from the systemd-managed bind-mount? Needs detail.
- **OQ-7**: Should we provide a `git-credential-helper`-compatible auxiliary binary (`remo-broker-git-credential`) so unmodified `git` works with broker-mediated credentials? Likely yes for v1.1; out of scope for the core daemon.
- **OQ-8**: How does the broker authenticate that an admin-socket caller is *Remo* vs. any-root-process-on-the-instance? In the current model, root is trusted absolutely. If we want defense-in-depth there (e.g., a shared secret in the admin handshake), the protocol needs extension.
