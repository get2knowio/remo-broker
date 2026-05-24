# `remo-broker` Wire Protocol

**Status**: Draft — protocol_version 1
**Audience**: in-devcontainer tooling (project-socket consumers), Remo's instance-side broker manager (admin-socket consumer), security reviewers.

The broker exposes two distinct Unix domain sockets, with different protocols, different audiences, and different security models. This document specifies both.

## 1. Common transport

Both sockets are `SOCK_STREAM` Unix domain sockets. Messages are framed as **newline-delimited JSON (NDJSON)**: one JSON object per message, terminated by a single `\n` byte. Embedded literal newlines inside JSON strings MUST be encoded as `\n` per the JSON spec.

Rationale: NDJSON is trivial to parse in every language (Python, Rust, shell + `jq`), is extensible without breaking older clients, and avoids the escaping headaches of a custom line protocol with secret values that could contain arbitrary bytes.

**Max message size**: 64 KiB per request and per response. Larger messages are rejected with an `error` response and the connection is closed.

**Encoding**: UTF-8 only. Non-UTF-8 bytes in a secret value MUST be base64-encoded; see the `value_b64` response field.

## 2. Project socket (data plane)

**Path**: `/run/remo-broker/<project_name>.sock`
**Mode**: `0660`, owner `remo-broker:remo-broker`, additional access via bind-mount into the devcontainer.
**Audience**: tools running inside the project's devcontainer (e.g., `gh`, `npm`, `aws`, `git credential helpers`, ad-hoc scripts).

### Authentication and attestation

The broker uses `SO_PEERCRED` on each accepted connection to identify the calling PID/UID/GID. Authorization decisions are made on the basis of:

1. The socket itself: only mounted into one devcontainer, so any caller is by-construction from that devcontainer.
2. The peer UID: connections from UIDs the broker does not expect (e.g., not the devcontainer's user) are logged as `peer.unexpected` and refused. Bypasses defensive coding errors on the bind-mount.

The broker does NOT attempt to attest the *binary* of the caller (path-based attestation is unreliable across container filesystems). The trust boundary is the devcontainer itself — anything running in it sees the same secrets.

### Operations

#### `get` — fetch a secret value

Request:

```json
{"op": "get", "name": "GITHUB_TOKEN"}
```

Response (success):

```json
{"ok": true, "value": "ghp_xxxxxxxxxxxxxxxxxxxx", "ttl_seconds": 542}
```

`ttl_seconds` is the remaining lifetime of the cached value the broker just returned (informational; clients should not cache).

Response (success, non-UTF-8 value):

```json
{"ok": true, "value_b64": "ZGVhZGJlZWY=", "ttl_seconds": 542}
```

Exactly one of `value` or `value_b64` is present on success.

Response (denied — name not in allowlist):

```json
{"ok": false, "code": "denied", "message": "Secret 'NPM_TOKEN' is not in this project's allowlist."}
```

Response (not found — name in allowlist but backend returns no such secret):

```json
{"ok": false, "code": "not_found", "message": "Backend has no secret named 'NPM_TOKEN'."}
```

Response (backend error — outage, auth failure, etc.):

```json
{"ok": false, "code": "backend_error", "message": "Backend 'vault' returned 503 (read-only mode)."}
```

Response (rate-limited — broker is throttling this socket):

```json
{"ok": false, "code": "rate_limited", "message": "Too many fetches in window.", "retry_after_seconds": 10}
```

#### `ping` — health check / liveness

Request:

```json
{"op": "ping"}
```

Response:

```json
{"ok": true, "broker_version": "0.3.1", "protocol_version": 1, "project": "myrepo"}
```

Useful for tools wanting to verify the broker is reachable before attempting a `get`.

#### `info` — manifest introspection

Request:

```json
{"op": "info"}
```

Response:

```json
{"ok": true, "project": "myrepo", "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN"], "schema_version": 1}
```

Lets tools display "this devcontainer is configured to access N secrets" without attempting fetches.

### Error codes (project socket)

| Code | Meaning | Retry? |
|---|---|---|
| `denied` | Name not in this project's allowlist. | No (manifest change required). |
| `not_found` | Name in allowlist; backend has no such secret. | No. |
| `backend_error` | Backend reachable but errored. | Maybe; respect `retry_after_seconds` if present. |
| `backend_unreachable` | Backend network failure; no cached value available. | Yes, with backoff. |
| `rate_limited` | Per-socket throttle triggered. | Yes, after `retry_after_seconds`. |
| `protocol_error` | Malformed request, unknown `op`, oversized message. | No (fix the client). |
| `internal_error` | Broker bug; details in broker log. | No. |
| `peer_unexpected` | Connection from unexpected UID. | No. |

### Concurrency

The broker handles project-socket connections concurrently (one tokio task per connection). Within a single connection, requests are processed sequentially in request order; pipelining is supported (clients may send multiple requests before reading responses) and responses arrive in request order.

## 3. Admin socket (control plane)

**Path**: `/run/remo-broker/admin.sock`
**Mode**: `0600`, owner `root:root` — accessible only to processes running as root on the instance.
**Audience**: Remo's instance-side broker manager (`remo` Ansible/CLI bits running on the instance), an `remo-broker-admin` CLI shipped alongside the daemon, manual sysadmin operations.

### Authentication

Filesystem permissions only — root-only by mode. A future protocol_version may add token-based auth for non-root callers; not in v1.

### Operations

#### `register` — bring a project online

Request:

```json
{"op": "register", "name": "myrepo", "project_path": "/projects/myrepo"}
```

Broker behavior:

1. Locate the manifest under `project_path` (`.devcontainer/remo-broker.toml` first, then `.remo/broker.toml`).
2. Validate it (see manifest-schema.md).
3. Create `/run/remo-broker/myrepo.sock` with mode `0660` and the appropriate ownership for the devcontainer.
4. Begin serving.

Response:

```json
{"ok": true, "socket_path": "/run/remo-broker/myrepo.sock"}
```

Or:

```json
{"ok": false, "code": "manifest_invalid", "message": "..."}
```

#### `unregister` — tear down a project

Request:

```json
{"op": "unregister", "name": "myrepo"}
```

Broker behavior: close listener, remove socket file, drop the project's allowlist and any cached values for that project's exclusive use from memory (cached values shared with other projects are retained).

Response:

```json
{"ok": true}
```

#### `reload` — reread a project's manifest

Request:

```json
{"op": "reload", "name": "myrepo"}
```

Used when the manifest file changes (e.g., the developer edited it). Broker re-parses, re-validates, and atomically swaps the in-memory allowlist. No socket teardown.

Response:

```json
{"ok": true, "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN", "ANTHROPIC_API_KEY"]}
```

#### `status` — list registered projects

Request:

```json
{"op": "status"}
```

Response:

```json
{
  "ok": true,
  "broker_version": "0.3.1",
  "protocol_version": 1,
  "uptime_seconds": 84273,
  "bootstrap_mode": "imds",
  "projects": [
    {"name": "myrepo", "socket_path": "/run/remo-broker/myrepo.sock", "allowlist_size": 3, "cache_entries": 2},
    {"name": "other",  "socket_path": "/run/remo-broker/other.sock",  "allowlist_size": 1, "cache_entries": 0}
  ]
}
```

#### `rotate-bootstrap` — replace the bootstrap token

Request:

```json
{"op": "rotate-bootstrap"}
```

Broker behavior: re-read the bootstrap token from its source (file or IMDS), re-authenticate to the backend, retain cached values across the rotation. Used by `remo rotate-bootstrap` on the laptop.

Response:

```json
{"ok": true, "backend_auth": "ok"}
```

### Error codes (admin socket)

| Code | Meaning |
|---|---|
| `manifest_invalid` | Manifest parse/validation failed; details in `message`. |
| `manifest_not_found` | No manifest at either path under `project_path`. |
| `project_unknown` | `unregister` or `reload` referenced an unregistered project. |
| `project_exists` | `register` called for a project name already registered. |
| `bootstrap_error` | `rotate-bootstrap` failed; broker continues with previous token. |
| `protocol_error` | Malformed request; see project-socket section. |
| `internal_error` | Broker bug. |

## 4. Versioning

Both sockets advertise `protocol_version` in `ping`/`status` responses. Clients SHOULD verify they understand the version before issuing other operations.

Within a major `protocol_version`, only additive changes are made (new optional request fields, new optional response fields, new error codes). Removing or renaming fields, changing semantics, or removing operations requires a major version bump.

When `protocol_version` increments, the broker accepts both the previous and new version on incoming requests for at least one minor release cycle.

## 5. Examples

### Devcontainer tool fetching a secret

```bash
# Inside the devcontainer, /run/remo-broker/sock is bind-mounted from the project socket
$ printf '{"op":"get","name":"GITHUB_TOKEN"}\n' | nc -U /run/remo-broker/sock
{"ok":true,"value":"ghp_xxxxxxxxxxxxxxxxxxxx","ttl_seconds":542}
```

### Remo registering a project from the instance

```bash
$ printf '{"op":"register","name":"myrepo","project_path":"/projects/myrepo"}\n' \
    | sudo nc -U /run/remo-broker/admin.sock
{"ok":true,"socket_path":"/run/remo-broker/myrepo.sock"}
```

### Status check

```bash
$ printf '{"op":"status"}\n' | sudo nc -U /run/remo-broker/admin.sock | jq
{
  "ok": true,
  "broker_version": "0.3.1",
  "protocol_version": 1,
  "uptime_seconds": 84273,
  "bootstrap_mode": "imds",
  "projects": [...]
}
```

## Open questions

- **OQ-W1**: Should the project socket support a `watch` operation that streams notifications when a secret's value changes upstream? Useful for long-running tools that hold credentials; adds significant complexity.
- **OQ-W2**: Should the admin socket support a streaming `tail-audit` for live audit-log monitoring? Trivial to add but adds a long-lived connection model the rest of the protocol doesn't have.
- **OQ-W3**: For the project socket, should we add an optional HMAC over each response signed by the broker so a compromised bind-mount path can be detected? Adds key-management complexity; the bind-mount is already trust-boundary-equivalent in the current model.
- **OQ-W4**: Should we offer a credential-helper-protocol-compatible socket alongside the JSON one (e.g., git-credential-helper, docker-credential-helper) so unmodified tools work directly? Possibly as a separate shim binary rather than in the broker itself.
