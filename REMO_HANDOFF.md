# `remo-broker` → Remo Integration Handoff

This document is for the [get2knowio/remo](https://github.com/get2knowio/remo)
team. It explains everything Remo needs to know to integrate the
`remo-broker` daemon — wire protocol, manifest format, bootstrap-token
lifecycle, instance provisioning, fnox-core's role, and the open questions
Remo still has to answer.

It is intentionally self-contained: you should be able to plan the Remo
side of the work from this single file without opening the broker's
internal spec (though pointers are at the bottom if you want them).

---

## Contents

- [The 30-second model](#the-30-second-model)
- [What Remo has to do](#what-remo-has-to-do)
- [Wire protocol](#wire-protocol)
  - [Common transport](#common-transport)
  - [Admin socket (control plane)](#admin-socket-control-plane)
  - [Project socket (data plane)](#project-socket-data-plane)
  - [Versioning rules](#versioning-rules)
- [Manifest format (Remo writes these)](#manifest-format-remo-writes-these)
- [Bootstrap-token lifecycle](#bootstrap-token-lifecycle)
- [fnox-core: what it is, who configures it](#fnox-core-what-it-is-who-configures-it)
- [Instance provisioning (what to install)](#instance-provisioning-what-to-install)
- [Operational concerns](#operational-concerns)
- [Open questions for Remo](#open-questions-for-remo)
- [Reference links](#reference-links)

---

## The 30-second model

```
┌──────────────────────────────────────────────────────────────────┐
│  An instance (EC2 / dev container host / wherever Remo runs)     │
│                                                                  │
│  ┌──────────────────────┐                                        │
│  │ remo-broker daemon   │                                        │
│  │                      │                                        │
│  │  /run/remo-broker/   │                                        │
│  │   admin.sock  ◄──── Remo's instance-side broker manager       │
│  │     (root only)     (register/unregister/reload/status/        │
│  │                      rotate-bootstrap)                         │
│  │                      │                                        │
│  │   <project>.sock ◄── Project devcontainer (bind-mounted)      │
│  │     (per project)   (get/ping/info)                            │
│  └──────────────────────┘                                        │
│           │                                                      │
│           ▼ (fnox-core, in-process)                              │
│       /etc/remo-broker/fnox.toml ──► 1Password / Vault / AWS /   │
│                                       age / OS keychain          │
└──────────────────────────────────────────────────────────────────┘
```

The broker is a long-lived Rust daemon. It:

1. Holds **one** short-lived bootstrap token (a string Remo mints).
2. Uses [fnox-core](https://github.com/jdx/fnox) to fetch secrets from
   whichever upstream credential store the operator configured.
3. Serves per-project Unix sockets. Each project has its own socket
   with its own **allowlist** of secret names. Off-allowlist requests are
   refused with no backend round-trip.
4. Writes every fetch attempt (allowed or denied) to a JSONL audit log.
   Values never appear in audit events.

**Remo's job is everything *around* the broker:** minting bootstrap
tokens, shipping them to instances, installing the daemon, writing
manifests that declare each project's allowlist, driving the admin
socket to register/reload/unregister projects, and rotating bootstrap
tokens on a schedule.

The broker has no opinions about how Remo does any of that — it just
implements the contract.

---

## What Remo has to do

A skeletal checklist. Each item links to a section below with the
detail.

### On the **laptop / Remo CLI** side

- [ ] **Mint bootstrap tokens** per instance. Format is opaque to the
      broker; whatever Remo chooses is fine. See
      [Bootstrap-token lifecycle](#bootstrap-token-lifecycle).
- [ ] **Synthesize manifests** for projects that don't ship one. Write
      them to `<project>/.remo/broker.toml`. Add to `.gitignore`. See
      [Manifest format](#manifest-format-remo-writes-these).
- [ ] **Push manifests + cache config** when the developer changes a
      project's allowed secrets. The instance-side manager then calls
      `reload` on the admin socket.

### On the **instance** (during provisioning)

- [ ] **Install the broker binary** to `/usr/bin/remo-broker`. Either
      bundle a release artifact or build from
      [github.com/get2knowio/remo-broker](https://github.com/get2knowio/remo-broker).
- [ ] **Install the systemd unit, sysusers, tmpfiles** from
      `packaging/` in the broker repo. See
      [Instance provisioning](#instance-provisioning-what-to-install).
- [ ] **Provision the bootstrap token** at
      `/etc/remo-broker/bootstrap-token` (or its TPM2-sealed `.cred`
      form). Mode `0600`, root-owned.
- [ ] **Provision `fnox.toml`** at `/etc/remo-broker/fnox.toml` (the
      path is configurable; this is the daemon's default for
      `--fnox-config`). See [fnox-core](#fnox-core-what-it-is-who-configures-it).
- [ ] **Enable the unit** (`systemctl enable --now remo-broker`).

### From the **instance-side broker manager** (runs as root, talks to admin.sock)

This is the new Remo subsystem. It owns admin-socket interactions.

- [ ] **`register`** projects when a developer adds one or when an
      instance comes up with pre-existing projects.
- [ ] **`reload`** when a manifest changes on disk.
- [ ] **`unregister`** when a developer removes a project.
- [ ] **`status`** for health checks / `remo broker status` CLI.
- [ ] **`rotate-bootstrap`** after writing a fresh token to
      `/etc/remo-broker/bootstrap-token`.

### From the **devcontainer side**

- [ ] **Bind-mount the project socket** into the devcontainer at a
      well-known path (e.g., `/run/remo-broker/sock`). The broker's
      side is `/run/remo-broker/<project_name>.sock`.
- [ ] Optionally **ship credential helpers** (a `git-credential-helper`
      wrapper, env-var injectors) so that unmodified tools inside the
      devcontainer can use broker-provided secrets transparently. The
      broker itself only speaks NDJSON over the socket — the helper
      layer is Remo's call.

### CI / release

- [ ] **Cross-repo integration test** (this repo's
      [SC-006](specs/001-broker-daemon/spec.md)) — wire the broker into
      Remo's existing CI matrix.
- [ ] **Pin to a broker version** (and eventually to its JSON Schema
      artifact for manifest validation; that artifact doesn't ship yet
      but is on the broker's roadmap).

---

## Wire protocol

Both sockets speak the same transport with different operations and
different audiences. Everything below is canonical against
[`docs/wire-protocol.md`](docs/wire-protocol.md) in the broker repo;
reproduced here so you don't need to round-trip.

### Common transport

- **Framing**: newline-delimited JSON (NDJSON). One JSON object per
  message, terminated by a single `\n`. Embedded newlines inside JSON
  strings are JSON-escaped as `\n`.
- **Max message size**: 64 KiB. Over-size messages are rejected and the
  connection is closed.
- **Encoding**: UTF-8 only. Non-UTF-8 secret values come back base64-
  encoded in a `value_b64` field instead of `value`.
- **Sockets**: `SOCK_STREAM` Unix domain. Same connection can carry
  multiple request/response pairs; the broker handles them serially per
  connection.
- **Concurrency**: the broker spawns one task per accepted connection;
  there is no global lock across projects. Multiple devcontainers can
  fetch concurrently.

### Admin socket (control plane)

- **Path**: `/run/remo-broker/admin.sock`
- **Mode**: `0600`, owner `root:root` (or `remo-broker:remo-broker`
  when the daemon is running as the `remo-broker` user — either way,
  only the daemon user and root can connect).
- **Audience**: Remo's instance-side broker manager.

#### `register`

```json
→ {"op": "register", "name": "myrepo", "project_path": "/projects/myrepo"}
← {"ok": true, "socket_path": "/run/remo-broker/myrepo.sock"}
```

- Loads + validates the manifest under `project_path` (tries
  `.devcontainer/remo-broker.toml` first, then `.remo/broker.toml`).
- Binds the project socket at mode `0660`, owner `remo-broker:remo-broker`.
- Idempotent only in the sense that calling it twice on the same name
  returns `project_exists`; the second call does *not* refresh the
  manifest (use `reload` for that).

Errors: `manifest_not_found`, `manifest_invalid`, `project_exists`,
`internal_error`.

#### `unregister`

```json
→ {"op": "unregister", "name": "myrepo"}
← {"ok": true}
```

- Removes the project socket file.
- Drops the project's cache entries (zeroized).
- In-flight `get` calls on the project socket complete or hit drain
  timeout (5 s) and are aborted.

Errors: `project_unknown`.

#### `reload`

```json
→ {"op": "reload", "name": "myrepo"}
← {"ok": true, "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN", "ANTHROPIC_API_KEY"]}
```

- Re-reads the manifest from disk for that project.
- Atomically swaps the in-memory allowlist (no fetch sees a half-loaded
  list).
- Does **not** drop the cache — previously fetched values remain valid
  for their TTL. This is intentional: reload is a metadata update, not a
  trust event.

Errors: `project_unknown`, `manifest_not_found`, `manifest_invalid`.

#### `status`

```json
→ {"op": "status"}
← {
    "ok": true,
    "broker_version": "0.1.0",
    "protocol_version": 1,
    "uptime_seconds": 84273,
    "bootstrap_mode": "file",   // or "imds" or "env"
    "projects": [
      {"name": "myrepo", "socket_path": "/run/remo-broker/myrepo.sock", "allowlist_size": 3, "cache_entries": 2},
      {"name": "other",  "socket_path": "/run/remo-broker/other.sock",  "allowlist_size": 1, "cache_entries": 0}
    ]
  }
```

No mutating side effects. Safe to poll.

#### `rotate-bootstrap`

```json
→ {"op": "rotate-bootstrap"}
← {"ok": true, "backend_auth": "ok"}
```

Sequence inside the broker:

1. Re-read the bootstrap token from whatever source was configured
   (`file` / `env` / `imds`).
2. Re-construct the fnox-core session (re-open `fnox.toml`).
3. Atomically swap the in-process Fnox handle.

On any failure the previous session is **retained**; the broker
continues serving cached values and uncached fetches against the old
session. Failure is returned to the caller as `bootstrap_error` with a
human-readable `message`.

Workflow for Remo:
- Write the new token to `/etc/remo-broker/bootstrap-token` (mode 0600).
- Call `rotate-bootstrap`.
- If the response is `bootstrap_error`, the broker is still running
  fine on the old token; investigate, fix, retry.

Errors: `bootstrap_error`.

#### Admin error codes

| Code | Meaning |
|---|---|
| `manifest_invalid` | TOML parse or validation failed. `message` carries details. |
| `manifest_not_found` | No `.devcontainer/remo-broker.toml` or `.remo/broker.toml` under `project_path`. |
| `project_unknown` | `unregister` or `reload` referenced a project that's not registered. |
| `project_exists` | `register` called for a project name already registered. |
| `bootstrap_error` | `rotate-bootstrap` failed; broker continues with previous session. |
| `protocol_error` | Malformed request, unknown `op`, oversized message. |
| `internal_error` | Broker bug. Check broker logs. |

### Project socket (data plane)

- **Path**: `/run/remo-broker/<project_name>.sock`
- **Mode**: `0660`, owner `remo-broker:remo-broker`. Bind-mounted into
  the devcontainer.
- **Audience**: tools running inside the project's devcontainer.

The broker uses `SO_PEERCRED` to record `peer_pid` and `peer_uid` in
audit events for each fetch. There is currently no policy enforcement
on UID — that's an [open question](#open-questions-for-remo).

#### `get`

```json
→ {"op": "get", "name": "GITHUB_TOKEN"}
← {"ok": true, "value": "ghp_xxxxxxxxxxxxxxxxxxxx", "ttl_seconds": 542}
```

Binary values come back base64-encoded:

```json
← {"ok": true, "value_b64": "ZGVhZGJlZWY=", "ttl_seconds": 542}
```

`ttl_seconds` is the **remaining** lifetime of the cached value. It's
informational — clients should not cache themselves.

Order of operations inside the broker:

1. **Allowlist check** (FR-012). Off-allowlist → `denied`, no backend
   round-trip, no cache lookup. Audit `decision=deny, reason=allowlist`.
2. **Cache lookup**. Hit → return; audit `decision=allow, backend=cache`.
3. **fnox-core fetch**. Returns plaintext → cache it (wrapped in a
   zeroize-on-drop `SecretString`) → return.

#### `ping`

```json
→ {"op": "ping"}
← {"ok": true, "broker_version": "0.1.0", "protocol_version": 1, "project": "myrepo"}
```

Liveness check. Not audited.

#### `info`

```json
→ {"op": "info"}
← {"ok": true, "project": "myrepo", "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN"], "schema_version": 1}
```

Tells the devcontainer what names it's allowed to request. Useful for
helper tooling that wants to validate a request before sending it. Not
audited.

#### Project socket error codes

| Code | Meaning | Retry? |
|---|---|---|
| `denied` | Name not in this project's allowlist. | No (manifest change required). |
| `not_found` | Name in allowlist but fnox-core returned `Ok(None)` (declared with `if_missing = "ignore"` / `"warn"`). | No. |
| `backend_error` | fnox-core returned `Err`. **Includes undeclared keys** — fnox treats undeclared as `Err`, not `Ok(None)`. | Maybe; honor `retry_after_seconds` if present. |
| `backend_unreachable` | (Reserved; not currently distinguished from `backend_error` because fnox-core doesn't expose the distinction.) | Yes, with backoff. |
| `rate_limited` | Per-socket throttle. | Yes after `retry_after_seconds`. |
| `protocol_error` | Malformed request, unknown `op`, oversized. | No. |
| `internal_error` | Broker bug. | No. |
| `peer_unexpected` | Connection from an unexpected UID. (Currently reserved — see [open questions](#open-questions-for-remo).) | No. |

### Versioning rules

- Both sockets advertise `protocol_version` in `ping` / `status`
  responses. Today: `1`.
- **Within a major version**: only additive changes. New optional
  request fields, new optional response fields, new error codes. The
  broker tolerates unknown request fields by design. This means Remo
  can ship a slightly newer client and an older broker, or vice versa,
  within v1.x.
- **Across major versions**: the broker accepts both old and new for at
  least one minor release.
- **JSON Schema artifact** (for manifests, not the wire protocol) ships
  per release as `schema/remo-broker.v1.json`. Remo should pin to a
  specific version for validation. *Note: this artifact is on the
  broker's roadmap but not shipped yet — track in the broker repo.*

---

## Manifest format (Remo writes these)

When Remo synthesizes a manifest for a project, it writes to
`<project>/.remo/broker.toml`. (If the project author ships a
hand-written `<project>/.devcontainer/remo-broker.toml`, the broker
prefers that and Remo's `.remo/broker.toml` is ignored. Remo should add
`.remo/broker.toml` to `.gitignore`.)

### Schema

```toml
schema_version = 1

[project]
name = "myrepo"                # required; must match the project directory basename
description = "..."            # optional; ≤256 chars; informational

[allowlist]
secrets = ["GITHUB_TOKEN", "NPM_TOKEN"]   # required; may be empty

[cache]                        # optional; per-project overrides
ttl_seconds = 3600             # 1..=86400; caps the broker-wide default downward
max_entries = 64               # 1..=1024; caps the broker-wide default downward
```

### Validation rules the broker enforces

1. Valid TOML 1.0.
2. `schema_version = 1` (broker rejects unknown versions).
3. `project.name` matches `^[a-z0-9][a-z0-9_-]{0,63}$`.
4. `project.name` matches the directory basename (defense against a
   hand-edited manifest claiming a different identity — Remo must
   ensure this invariant when synthesizing).
5. Each `allowlist.secrets[]` entry matches `^[A-Za-z0-9_]{1,128}$`;
   no duplicates.
6. `cache.*` numbers in range, and **cannot raise** broker-wide
   defaults (only lower them — security floor).
7. No unknown top-level tables and no unknown keys (strict). Future
   `[experimental]` table may relax this; track in broker spec.

### Minimal example (what Remo's auto-synthesizer probably emits)

```toml
schema_version = 1

[project]
name = "myrepo"

[allowlist]
secrets = ["GITHUB_TOKEN"]
```

---

## Bootstrap-token lifecycle

The bootstrap token is **opaque to the broker**. It's a string the
broker holds and may pass to fnox-core's providers if they want it.
What it represents is Remo's choice:

- It could be a long-lived AWS/Vault/etc. credential.
- It could be a short-lived JWT Remo mints and rotates frequently.
- It could be empty if fnox-core's providers don't need a bootstrap
  credential (e.g., they use IMDS).

The broker just resolves it from one of three sources at startup (and
again on `rotate-bootstrap`):

| Source | How the broker reads it | When to use |
|---|---|---|
| `file` (default) | Reads `--bootstrap-token-path` (default `/etc/remo-broker/bootstrap-token`). | Normal production. Use systemd `LoadCredential=` (plaintext) or `LoadCredentialEncrypted=` (TPM2-sealed). |
| `imds` | AWS IMDSv2 (`http://169.254.169.254`): PUT token → GET role → GET credentials JSON. | EC2 instances with IAM roles; the broker wraps the credentials JSON verbatim. |
| `env` | `$REMO_BROKER_BOOTSTRAP_TOKEN`. | Development only. The broker logs a warning at startup. |

Lifecycle Remo owns:

1. **Mint** at provisioning. Length / format / signing is Remo's call.
2. **Deliver** to the instance. For TPM2-sealed hosts, encrypt with
   `systemd-creds encrypt --name=bootstrap-token` on the target and
   ship the `.cred` file. For plaintext, write to
   `/etc/remo-broker/bootstrap-token` with mode `0600`.
3. **Rotate** on a schedule (or on suspicion). Write the new token to
   the same path, then send `rotate-bootstrap` to the admin socket.
   The broker drops the old session only if the new one constructs
   successfully (User Story 5 scenario 3 — failure is non-disruptive).
4. **Revoke** at decommission. Delete the token file *and* stop the
   broker (`systemctl stop remo-broker`). The broker doesn't need a
   "revoke" admin op — file deletion + restart is the path.

Open question for Remo: rotation cadence + the upstream coordination
(if fnox-core's providers refresh credentials independently, how often
does the bootstrap token itself need to rotate?). See
[OQ-2 in the broker spec](specs/001-broker-daemon/spec.md#deferred-work-and-roadmap)
— it was resolved on the broker side as "no auto-refresh in the broker;
fnox-core handles credential rotation internally." So Remo controls
broker-token cadence; fnox-core controls upstream credential cadence.

---

## fnox-core: what it is, who configures it

[fnox-core](https://github.com/jdx/fnox) is the secret-resolution
engine the broker links against. It abstracts over the actual
credential stores (1Password, Vault, AWS Secrets Manager, age,
keychain, plain-file for dev). The broker passes secret *names* to
fnox-core and receives `Result<Option<String>>`. The broker has zero
backend-specific logic of its own.

**Who configures fnox.toml?** Remo or the operator, not the broker.
The broker is given a `--fnox-config /path/to/fnox.toml` flag (or
falls back to `Fnox::discover()` which walks for `fnox.toml` per the
fnox CLI's rules). What's in that file determines:

- Which provider each secret name routes to.
- How each provider authenticates upward.
- Per-secret `if_missing` policy (which determines whether absent
  values surface as `not_found` or `backend_error` on the broker side).

Typical Remo provisioning would write `/etc/remo-broker/fnox.toml`
based on the developer's chosen credential backend ("I use 1Password",
"I use Vault"). Format is fnox's own; see
[jdx/fnox](https://github.com/jdx/fnox) for the schema.

**Minimal `fnox.toml` for a hermetic test** (used in the broker's
own CONTRIBUTING playbook):

```toml
[providers]
plain = { type = "plain" }

[secrets]
HELLO = { provider = "plain", value = "world" }
```

This is dev-only — the `plain` provider stores values in cleartext in
the config file. Real deployments route to real backends.

**Degraded mode.** If the daemon starts and can neither `open()` a
provided path nor `discover()` a `fnox.toml`, it logs a warning and
runs without a backend. Admin/ping/info/cache-hit traffic still works;
cache-miss `get` returns `backend_error` with a hint to set
`--fnox-config`. Remo's provisioning should detect this in `status`
output (`bootstrap_mode` and a follow-up `get` test would tell you).

---

## Instance provisioning (what to install)

The broker repo ships ready-to-use systemd unit + sysusers + tmpfiles
files in `packaging/`. Remo's provisioning should lay them down at:

| Source (broker repo) | Install location | Purpose |
|---|---|---|
| `packaging/systemd/remo-broker.service` | `/etc/systemd/system/remo-broker.service` | The unit. `Type=notify` with full hardening (`ProtectSystem=strict`, `NoNewPrivileges=yes`, `MemoryDenyWriteExecute=yes`, `SystemCallFilter=@system-service`, `CapabilityBoundingSet=`, etc.). |
| `packaging/sysusers.d/remo-broker.conf` | `/usr/lib/sysusers.d/remo-broker.conf` | Creates the `remo-broker` system user/group. |
| `packaging/tmpfiles.d/remo-broker.conf` | `/usr/lib/tmpfiles.d/remo-broker.conf` | `/run/remo-broker` + `/var/log/remo-broker` for ad-hoc (non-systemd) runs. |

Plus the binary at `/usr/bin/remo-broker` (or wherever — adjust
`ExecStart=`) and the two config files at `/etc/remo-broker/`.

The unit ships with `LoadCredential=bootstrap-token:/etc/remo-broker/bootstrap-token`
(plaintext default) and `LoadCredentialEncrypted=` commented as a
TPM2-sealed alternative.

**Runtime dependency**: fnox-core's transitive `hidapi` dep (for
YubiKey/WebAuthn provider support) dynamically links `libudev1`. Most
distros already have it because systemd depends on it; on Debian/Ubuntu
the package is `libudev1`. The eventual `.deb` for the broker should
declare it as a `Depends:`.

After install: `systemctl daemon-reload && systemctl enable --now remo-broker`.

---

## Operational concerns

### Failure modes Remo's manager should handle

| Symptom | Likely cause | Remo's response |
|---|---|---|
| `register` returns `manifest_not_found` | Project dir has no `.devcontainer/remo-broker.toml` or `.remo/broker.toml`. | Either Remo hasn't synthesized one yet, or the path is wrong. |
| `register` returns `manifest_invalid` | Validation failed (name mismatch, bad TOML, etc.). | `message` tells you what. Re-synthesize. |
| `register` returns `project_exists` | The project is already registered. | Probably benign — confirm via `status`. If genuinely a re-register intent, `unregister` first then `register`. |
| `get` returns `backend_error` with `--fnox-config` hint | Daemon started in degraded mode. | Provision `/etc/remo-broker/fnox.toml` and `systemctl restart remo-broker`. |
| `get` returns `backend_error` with a fnox-core message | fnox couldn't resolve the name (often: the name isn't in `fnox.toml`). | Verify the secret is declared in `fnox.toml`; align `fnox.toml` and the project manifest's allowlist. |
| `rotate-bootstrap` returns `bootstrap_error` | New token unreadable, or new `fnox.toml` won't open. | Old session is **retained** — broker keeps serving. Investigate, fix, retry. Don't restart. |
| Admin socket connection refused | Daemon not running, or running as a user without permission to read the socket. | `systemctl status remo-broker` + `journalctl -u remo-broker`. |

### Audit log

Path: `/var/log/remo-broker/audit.log` (configurable). One JSONL line
per fetch attempt. Schema example:

```json
{"event":"fetch","timestamp":"2026-05-24T16:13:42.140Z","project":"myrepo","secret_name":"GITHUB_TOKEN","decision":"allow","outcome":"ok","peer_pid":12345,"peer_uid":1000,"latency_ms":3,"backend":"fnox"}
```

- `decision`: `allow` | `deny`
- `outcome`: `ok` | `not_found` | `backend_error` | `backend_unreachable`
- `backend`: `fnox` (cache miss → real fetch) | `cache` (cache hit) |
  absent (denied — no backend involved)
- `reason`: present on denials (`"allowlist"` today)
- **`value` never appears.** Structural guarantee; verified by the
  broker's `audit_never_contains_secret_value` test.

`ping` / `info` are not audited (FR-013 is for fetches only).

### Shutdown behavior

- SIGTERM → broker stops accepting new connections, drains in-flight up
  to 5 s, removes all socket files, exits 0.
- Remo's manager should expect that immediately after `systemctl stop`,
  every project socket is gone. A subsequent `register` will re-bind it.

### What survives a daemon restart

Nothing on disk except the audit log. The cache is in-memory only
(FR-015); restart loses cached values. This is by design — restart is
acceptable; cache loss isn't. Remo doesn't have to do anything special
for restart, but be aware that the post-restart latency profile is
"cold cache" for a while.

---

## Open questions for Remo

These are decisions the broker can't make for you. Each affects Remo's
integration shape.

1. **Manifest source of truth.** Are `.devcontainer/remo-broker.toml`
   (committed) and `.remo/broker.toml` (auto-synthesized) both
   supported in Remo's UI, or is one of them the canonical path? The
   broker accepts either.
2. **Bootstrap-token format and minting.** The broker doesn't care
   what's in the file — Remo decides. Likely candidates: a JWT signed
   by Remo's control plane; a long-lived secret from a vault; an AWS
   STS session.
3. **Bootstrap rotation cadence.** Daily? Weekly? On-demand only?
   Whatever Remo picks, the mechanism is the same:
   write-token-then-`rotate-bootstrap`.
4. **Devcontainer → broker integration shape.** Three options:
   (a) ship credential-helper wrappers (e.g., a `git-credential-remo`
   binary), (b) inject env vars at devcontainer start, (c) make the
   socket discoverable and let tools speak NDJSON directly. Mix-and-
   match likely.
5. **Project-socket UID enforcement (OQ-6 in the broker spec).** Today
   the broker records `peer_uid` but enforces nothing. Should off-UID
   connections be refused? Remo knows what UIDs are legitimate for each
   devcontainer; the broker doesn't. Two paths: Remo configures an
   expected UID per project, or Remo accepts "anything in the
   devcontainer is in scope" (current behavior).
6. **`fnox.toml` provisioning model.** Is `fnox.toml` machine-managed
   (Remo writes it based on the developer's backend choice) or
   operator-managed (the developer / sysadmin hand-edits it)? Affects
   how `remo init` / `remo configure-backend` should work.
7. **Cross-repo CI.** Remo's CI matrix needs the broker binary and a
   sample `fnox.toml` to exercise SC-006 end-to-end. The broker's
   CONTRIBUTING playbook has a working `plain`-provider example to
   copy from.
8. **JSON Schema artifact pin.** When the broker starts shipping
   `schema/remo-broker.v1.json` (roadmap item), Remo should validate
   manifests against it client-side rather than relying on the broker
   round-trip. Coordinate version-pin policy.

---

## Reference links

In the broker repo (`get2knowio/remo-broker`):

- **[`README.md`](README.md)** — high-level overview + working
  hello-world.
- **[`CONTRIBUTING.md`](CONTRIBUTING.md)** — has a 17-scenario
  end-to-end verification playbook you can run against a built binary.
  Useful for Remo as a reference implementation of an admin-socket
  client and a project-socket client (all in shell + `socat`).
- **[`docs/wire-protocol.md`](docs/wire-protocol.md)** — the canonical
  protocol spec, slightly more detail than reproduced here.
- **[`docs/manifest-schema.md`](docs/manifest-schema.md)** — the
  canonical manifest spec.
- **[`packaging/README.md`](packaging/README.md)** — operator install
  guide; useful when designing Remo's provisioning module.
- **[`packaging/systemd/remo-broker.service`](packaging/systemd/remo-broker.service)**
  — the actual unit file Remo will install.
- **[`specs/001-broker-daemon/spec.md`](specs/001-broker-daemon/spec.md)**
  — the full feature spec, with FR/NFR tables, deferred-work roadmap,
  and 30+ rows of non-obvious implementation decisions.

External:

- **[jdx/fnox](https://github.com/jdx/fnox)** — fnox-core's docs;
  authoritative for `fnox.toml` syntax and provider configuration.
- **Remo spec** (in your own repo, presumably):
  `specs/005-credential-broker/spec.md`. The broker repo references
  it as the source of integration requirements but doesn't reproduce
  its contents.

---

*This handoff is current as of broker commit
[`feacb4a`](https://github.com/get2knowio/remo-broker/commit/feacb4a).
The broker is pre-release (no tagged versions yet); the integration
contract above is stable per the wire-protocol versioning rules in
[§Versioning](#versioning-rules) but the binary distribution / .deb
story is still TBD on the broker side.*
