# Contributing to `remo-broker`

Thanks for taking an interest. This document covers:

- [Reporting issues](#reporting-issues)
- [Development setup](#development-setup)
- [The local CI pipeline](#the-local-ci-pipeline)
- [Repository conventions](#repository-conventions)
- [**Manual verification playbook**](#manual-verification-playbook) — copy-pasteable scenarios that exercise every user-visible feature; useful both for code review and for shaking out regressions before opening a PR
- [Code review checklist](#code-review-checklist)

If you're new to the codebase, the
[spec](specs/001-broker-daemon/spec.md) is the source of truth and the
[Key Implementation Decisions](specs/001-broker-daemon/spec.md#key-implementation-decisions)
table is the non-obvious-decisions index. Skim both before non-trivial
changes.

## Reporting issues

- **Bugs / feature requests**: open a regular GitHub issue.
- **Security vulnerabilities**: open a
  [private security advisory](https://github.com/get2knowio/remo-broker/security/advisories/new)
  rather than a public issue.
- **Spec ambiguities**: open an issue tagged `spec` — these usually
  resolve to a spec edit, not a code change.

## Development setup

### Toolchain

Rust **1.95** (pinned in `rust-toolchain.toml`). `rustup` will pick it up
automatically the first time you `cargo build`.

### Linux build dependencies

`fnox-core` pulls in `hidapi` transitively (for YubiKey / WebAuthn
provider support via `ctap-hid-fido2`), which links against `libudev`:

```bash
sudo apt-get install -y pkg-config libudev-dev
# also useful for the verification playbook:
sudo apt-get install -y socat
```

macOS and Windows are not supported build targets — the daemon uses
Linux-specific primitives (`SO_PEERCRED`, systemd sockets, IMDSv2).

### Build

```bash
cargo build               # debug
cargo build --release     # optimized; what packaging ships
```

## The local CI pipeline

Run the same checks CI does, in order. The whole sequence on an
already-warm build cache is a few seconds; from scratch is a few
minutes (fnox-core pulls in the AWS SDK).

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check
# systemd unit lint — needs systemd-analyze installed (Ubuntu: `sudo apt-get install -y systemd`)
sudo install -m 0755 /bin/true /usr/bin/remo-broker   # stub the exec-check
systemd-analyze verify packaging/systemd/remo-broker.service
```

PRs are gated on all of these. The full CI workflow is at
[`.github/workflows/ci.yml`](.github/workflows/ci.yml).

## Repository conventions

### Testing

- Tests live in `mod tests` inside the module under test. No
  `tests/` integration directory (yet).
- Hand-rolled `TempDir` per module — we don't take a `tempfile`
  crate dep.
- Env-mutating tests use per-test unique variable names because
  `std::env::set_var` is `unsafe` in edition 2024.
- Wire-protocol response tests compare serialized JSON to a `json!`
  literal copied from `docs/wire-protocol.md`. Pins against silent
  serde drift; regressions cite the doc.
- New behaviour comes with a new test. New error variants come
  with a test that constructs them.

### Code style

- `cargo fmt` is authoritative. CI checks `--check`.
- `cargo clippy --all-targets -- -D warnings` must pass. Don't
  `#[allow(...)]` to silence; either fix or argue the suppression
  in a comment and the PR description.
- No new dependencies without a one-line justification in the PR
  description. Each new dep is a license + advisory surface.
- Default to no comments. Add one when the *why* is non-obvious
  (a hidden constraint, a subtle invariant, a workaround). Don't
  explain *what* the code does; well-named identifiers do that.

### Commit messages

- Imperative subject under ~70 characters.
- A body that explains *why* (and any non-obvious *what*). The git
  history is a load-bearing artifact — `git log --oneline` should
  read like a changelog.
- Group related changes into one commit; don't atomize trivial
  formatting fixes into their own commits unless they're genuinely
  separable.
- Look at the existing log (`git log --oneline -20`) for the
  established voice and structure.

### Cargo.toml + deny.toml

- Bumping a dep version: re-run `cargo deny check` and `cargo audit`.
- Six RUSTSEC advisories are intentionally ignored — each has a
  one-line rationale in [`deny.toml`](deny.toml). When a dep bump
  resolves an advisory, remove its ignore entry. When a bump
  introduces a new one, the PR has to either upgrade past it or
  add a documented ignore.

## Manual verification playbook

This is a top-to-bottom hands-on smoke test. Build first
(`cargo build --release`), then run the scenarios in order — they
build on a shared `/tmp/rb` sandbox.

Every scenario shows the command, the expected response, and what
it proves.

### 0. Sandbox setup

```bash
rm -rf /tmp/rb /tmp/hello /tmp/other
mkdir -p /tmp/rb/{run,log}
echo "dev-bootstrap-token" > /tmp/rb/bootstrap-token

cat > /tmp/rb/fnox.toml <<'EOF'
[providers]
plain = { type = "plain" }

[secrets]
HELLO         = { provider = "plain", value = "world" }
NPM_TOKEN     = { provider = "plain", value = "npm_test_token_xxx" }
TRIPWIRE      = { provider = "plain", value = "DO-NOT-LEAK-tripwire-9f8a" }
EOF

# Project A: allowlist = [HELLO, NPM_TOKEN]
mkdir -p /tmp/hello/.remo
cat > /tmp/hello/.remo/broker.toml <<'EOF'
schema_version = 1

[project]
name = "hello"

[allowlist]
secrets = ["HELLO", "NPM_TOKEN"]
EOF

# Project B: allowlist = [TRIPWIRE]
mkdir -p /tmp/other/.remo
cat > /tmp/other/.remo/broker.toml <<'EOF'
schema_version = 1

[project]
name = "other"

[allowlist]
secrets = ["TRIPWIRE"]
EOF
```

### 1. Daemon starts cleanly

```bash
./target/release/remo-broker \
  --bootstrap-token-path /tmp/rb/bootstrap-token \
  --fnox-config          /tmp/rb/fnox.toml \
  --socket-dir           /tmp/rb/run \
  --audit-log-path       /tmp/rb/log/audit.log \
  &
BROKER_PID=$!
sleep 0.3
```

**Proves**: bootstrap-token resolution (FR-002 file), fnox-core
session construction (FR-004), admin socket binding (FR-006).

```bash
echo '{"op":"status"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true,"broker_version":"…","protocol_version":1,"uptime_seconds":…,"bootstrap_mode":"file","projects":[]}
```

**Proves**: admin protocol round-trip (FR-019/020), versions
advertised, no projects yet.

### 2. Register both projects

```bash
echo '{"op":"register","name":"hello","project_path":"/tmp/hello"}' \
  | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true,"socket_path":"/tmp/rb/run/hello.sock"}

echo '{"op":"register","name":"other","project_path":"/tmp/other"}' \
  | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true,"socket_path":"/tmp/rb/run/other.sock"}

ls -l /tmp/rb/run
# srw------- … admin.sock      (mode 0600 — root-only in production)
# srw-rw---- … hello.sock      (mode 0660)
# srw-rw---- … other.sock      (mode 0660)
```

**Proves**: manifest validation (FR-010), project socket binding
with mode 0660 (FR-007).

### 3. Duplicate register is refused

```bash
echo '{"op":"register","name":"hello","project_path":"/tmp/hello"}' \
  | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":false,"code":"project_exists","message":"…"}
```

**Proves**: no silent replacement of a registered project.

### 4. Missing manifest is refused

```bash
mkdir -p /tmp/nope
echo '{"op":"register","name":"nope","project_path":"/tmp/nope"}' \
  | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":false,"code":"manifest_not_found","message":"…"}
```

### 5. Project socket: ping + info

```bash
echo '{"op":"ping"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":true,"broker_version":"…","protocol_version":1,"project":"hello"}

echo '{"op":"info"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":true,"project":"hello","allowlist":["HELLO","NPM_TOKEN"],"schema_version":1}
```

### 6. Get an allowed, fnox-resolvable secret

```bash
echo '{"op":"get","name":"HELLO"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":true,"value":"world","ttl_seconds":900}
```

**Proves**: allowlist passes, fnox-core resolves, cache populated,
value returned (FR-004/012/014).

### 7. The second get is a cache hit (lower TTL, same value)

```bash
sleep 2
echo '{"op":"get","name":"HELLO"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":true,"value":"world","ttl_seconds":897}   # 2–3s lower than 900
```

**Proves**: cache hit short-circuits the backend; TTL reflects
elapsed time (FR-014). On a busy machine the exact number drifts
a second or two; what matters is that it's lower than 900.

### 8. Get an off-allowlist name → denied

```bash
echo '{"op":"get","name":"GITHUB_TOKEN"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":false,"code":"denied","message":"…"}
```

**Proves**: allowlist check happens before backend (FR-012).

### 9. Reload, then get an allowed name fnox can't resolve

Add an allowlist entry for a name fnox doesn't define, `reload`,
then fetch.

```bash
cat > /tmp/hello/.remo/broker.toml <<'EOF'
schema_version = 1

[project]
name = "hello"

[allowlist]
secrets = ["HELLO", "NPM_TOKEN", "UNDEFINED"]
EOF

echo '{"op":"reload","name":"hello"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true,"allowlist":["HELLO","NPM_TOKEN","UNDEFINED"]}

echo '{"op":"get","name":"UNDEFINED"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":false,"code":"backend_error","message":"fnox-core: Secret 'UNDEFINED' not found in profile 'default' …"}
```

The broker's two surface codes for "couldn't fetch" map to fnox-core's
return shape:

- `backend_error` — fnox-core returned `Err`. **This is what you get
  for a name that simply isn't declared in `fnox.toml`** — fnox treats
  an undeclared key as an error, not as a missing value.
- `not_found` — fnox-core returned `Ok(None)`. Only fires for keys
  that *are* declared in `fnox.toml` but use `if_missing = "ignore"`
  or `"warn"` and have no value at resolve time.

So the case in this scenario (an allowlist name with no matching
`[secrets]` entry) always lands in `backend_error`. To exercise the
`not_found` path you'd need an `if_missing`-marked secret in
`fnox.toml`.

**Proves**: reload picks up the new allowlist atomically (FR-011);
fnox-core errors surface as `backend_error` with the underlying
message attached for operator triage.

### 10. Status reflects current state

```bash
echo '{"op":"status"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock | jq .
# → projects: [
#     {"name":"hello", "allowlist_size":3, "cache_entries":1, …},
#     {"name":"other", "allowlist_size":1, "cache_entries":0, …}
#   ]
```

**Proves**: status reports projects + cache occupancy (FR-019/020).

### 11. Audit log captured every fetch, with no values

```bash
cat /tmp/rb/log/audit.log | jq .
# → one JSONL line per fetch: ping/info do NOT appear
```

Eyeball that each fetch line has `decision`, `outcome`, `peer_pid`,
`peer_uid`, `latency_ms`, and *no* `value` field.

Spot-check that the `HELLO` fetches (scenarios 6 + 7) didn't leak
the plaintext "world" into the log:

```bash
grep -F "world" /tmp/rb/log/audit.log; echo "exit=$?"
# → exit=1   (no match)
```

**Proves**: SC-004 audit-log half — no secret value ever reaches
the audit log. (The structural guarantee — `FetchEvent` has no
`value` field — is verified in CI by the
`audit_never_contains_secret_value` test.)

### 12. Unregister tears down

```bash
echo '{"op":"unregister","name":"hello"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true}

ls /tmp/rb/run/hello.sock 2>&1
# → No such file or directory

echo '{"op":"status"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock | jq '.projects | length'
# → 1
```

**Proves**: project socket removed (FR-008), cache implicitly
dropped (cache lives on the Project Arc that just got unregistered;
this also drops the SecretString entries — FR-016 zeroize-on-drop).

### 13. rotate-bootstrap success path

```bash
echo "rotated-bootstrap-token" > /tmp/rb/bootstrap-token

echo '{"op":"rotate-bootstrap"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":true,"backend_auth":"ok"}

# The remaining "other" project still works:
echo '{"op":"get","name":"TRIPWIRE"}' | socat - UNIX-CONNECT:/tmp/rb/run/other.sock
# → {"ok":true,"value":"DO-NOT-LEAK-tripwire-9f8a","ttl_seconds":900}

# And the audit log still doesn't contain the value we just fetched:
grep -F "DO-NOT-LEAK-tripwire-9f8a" /tmp/rb/log/audit.log; echo "exit=$?"
# → exit=1   (no match)
```

**Proves**: rotate re-reads token, swaps Fnox session atomically,
existing project sockets keep working (FR-005, User Story 5);
SC-004 still holds after the rotation path.

### 14. rotate-bootstrap with a broken fnox.toml → keeps old session

```bash
mv /tmp/rb/fnox.toml /tmp/rb/fnox.toml.bak
echo "this is not valid toml [[" > /tmp/rb/fnox.toml

echo '{"op":"rotate-bootstrap"}' | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
# → {"ok":false,"code":"bootstrap_error","message":"failed to rebuild fnox-core session: …"}

# old session still serves cached values:
echo '{"op":"get","name":"TRIPWIRE"}' | socat - UNIX-CONNECT:/tmp/rb/run/other.sock
# → {"ok":true,"value":"DO-NOT-LEAK-tripwire-9f8a", …}

mv /tmp/rb/fnox.toml.bak /tmp/rb/fnox.toml
```

**Proves**: User Story 5 scenario 3 — failed rotation keeps the
previous session.

### 15. Clean shutdown

```bash
kill -TERM $BROKER_PID
wait $BROKER_PID; echo "exit=$?"
# → exit=0 (within ~5s)

ls /tmp/rb/run
# → empty — admin and all project sockets removed (FR-008)
```

**Proves**: SIGTERM drain (FR-022) + socket cleanup on exit.

### 16. Degraded mode (no fnox-config)

```bash
./target/release/remo-broker \
  --bootstrap-token-path /tmp/rb/bootstrap-token \
  --socket-dir           /tmp/rb/run \
  --audit-log-path       /tmp/rb/log/audit.log \
  &
BROKER_PID=$!
sleep 0.3

# register + get against an allowed name → backend_error with hint
echo '{"op":"register","name":"hello","project_path":"/tmp/hello"}' \
  | socat - UNIX-CONNECT:/tmp/rb/run/admin.sock
echo '{"op":"get","name":"HELLO"}' | socat - UNIX-CONNECT:/tmp/rb/run/hello.sock
# → {"ok":false,"code":"backend_error","message":"… --fnox-config …"}

kill -TERM $BROKER_PID; wait $BROKER_PID
```

**Proves**: missing fnox.toml doesn't block daemon startup;
operator gets a targeted hint instead of an opaque error.

### 17. (Optional) systemd unit lint

If you have `systemd-analyze` installed:

```bash
sudo install -m 0755 /bin/true /usr/bin/remo-broker
systemd-analyze verify packaging/systemd/remo-broker.service; echo "exit=$?"
# → exit=0
```

**Proves**: unit syntax is valid.

## Code review checklist

For reviewers:

- [ ] Tests cover the new behaviour (both happy path and at least one
      failure mode). For new error variants, a test constructs them.
- [ ] No `#[allow(...)]` introduced without a comment explaining why.
- [ ] No new dependency without a one-line justification in the PR
      description. New deps run `cargo deny check` clean (or the
      `deny.toml` change is part of the PR with rationale).
- [ ] If wire-protocol types changed, `docs/wire-protocol.md` is
      updated in the same PR.
- [ ] If manifest types changed, `docs/manifest-schema.md` is
      updated in the same PR.
- [ ] If a spec FR/NFR/SC moved status, the dashboard in
      `specs/001-broker-daemon/spec.md` is updated in the same PR.
- [ ] Commit message body explains *why*, not just *what*.
- [ ] No backend-specific (1Password / Vault / AWS / age / keychain)
      logic outside `src/backend.rs`. FR-004 says everything routes
      through fnox-core.
- [ ] No `value` field added to any `AuditEvent` variant. FR-017 is
      a structural guarantee.

## License

By contributing you agree your contributions will be licensed under
the MIT License (see [`LICENSE`](LICENSE)).
