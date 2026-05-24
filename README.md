# remo-broker

> On-instance credential broker daemon for [Remo](https://github.com/get2knowio/remo).

`remo-broker` holds a per-instance bootstrap token, authenticates upward to a
credential backend through [`fnox-core`](https://github.com/jdx/fnox), and
serves per-project Unix sockets that enforce per-project secret allowlists.
It is the on-instance half of Remo's credential-broker feature.

The full specification lives at [`specs/001-broker-daemon/spec.md`](specs/001-broker-daemon/spec.md);
this README is a tour for first-time readers and an entry point for operators
and contributors.

## Why does this exist?

Developer instances increasingly run untrusted code (npm/pip postinstall
scripts, MCP servers, LLM agents). Any long-lived credential sitting in
`~/.netrc`, `$GITHUB_TOKEN`, or `~/.aws/credentials` is reachable by that
code.

`remo-broker` removes those credentials from the instance. Instead:

- The daemon holds one short-lived **bootstrap token** that authenticates
  upward to a credential backend (1Password / Vault / AWS Secrets Manager /
  age / OS keychain — chosen by `fnox-core` config, not by the broker).
- Each project gets a **dedicated Unix socket** bind-mounted into its
  devcontainer.
- Each project carries an **allowlist** declaring which secret *names* it
  may fetch. Anything off the allowlist is refused with no backend round
  trip and an audit-log line.
- Every fetch attempt — allowed or denied — is written to an append-only
  JSONL audit log.

A successful supply-chain attack on a devcontainer can recover at most the
secrets that project's manifest explicitly named. Cross-project escalation
through the broker is structurally impossible: each socket is a separate
file with its own allowlist.

## Status

**Active development.** As of `c2717ed`, every functional requirement in
the spec is **Done**: bootstrap-token resolution (file / env / IMDSv2),
fnox-core integration, project registry with manifest validation,
per-project sockets with allowlist enforcement, per-project bounded
in-memory cache with zeroize-on-drop, append-only JSONL audit log with
peer credentials, atomic `rotate-bootstrap`, systemd unit + hardening.

125 tests pass; `cargo clippy --all-targets -- -D warnings`,
`cargo deny check`, and `systemd-analyze verify` all run clean in CI.

What's *not* done: latency / RSS / binary-size measurement (NFR-001
through NFR-005 are **Unverified**), the test harnesses for fuzz / soak /
killtest / red-team, the JSON Schema artifact for downstream pinning, and
some operational edges (project-socket group ownership, `peer_unexpected`
enforcement policy). See the
[deferred-work roadmap](specs/001-broker-daemon/spec.md#deferred-work-and-roadmap)
for the full list, in order.

## How it works

```
                                          +-------------------+
                                          |  fnox-core        |
                                          |  (1Password,      |
                                          |   Vault, AWS, …)  |
                                          +---------^---------+
                                                    |
                                          +---------+---------+
   /run/remo-broker/admin.sock  <--->     |                   |
   (root only, mode 0600)         control |   remo-broker     |
                                          |   daemon          |
   /run/remo-broker/<project>.sock <--->  |                   |
   (mode 0660, bind-mounted into          |                   |
    each devcontainer)             fetch  +---------+---------+
                                                    |
                                          +---------v---------+
                                          |  audit.log        |
                                          |  (JSONL,          |
                                          |   /var/log/…)     |
                                          +-------------------+
```

- **Admin socket** carries the control plane: `register` / `unregister` /
  `reload` / `status` / `rotate-bootstrap`. Owned by root, mode `0600`.
  Typically driven by Remo's instance-side broker manager.
- **Project sockets** carry the data plane: `get` / `ping` / `info`.
  One per registered project, mode `0660`, bind-mounted into that
  project's devcontainer.
- **Manifest** (`.devcontainer/remo-broker.toml` or `.remo/broker.toml`)
  declares the project's name and allowlist. The broker reads it at
  `register` and re-reads on `reload` — atomically swapped so no
  in-flight fetch sees a half-loaded allowlist.
- **Cache** is per-project, in-memory only, bounded by TTL and
  `max_entries` from the manifest. Values are wrapped in
  `secrecy::SecretString`; cache drop / eviction / project unregister
  zeroize.
- **Audit** is a single append-only JSONL file. Every fetch attempt is
  one line. Values never appear in audit events by construction (the
  event struct has no `value` field).

Wire-protocol details live in [`docs/wire-protocol.md`](docs/wire-protocol.md).
Manifest schema lives in [`docs/manifest-schema.md`](docs/manifest-schema.md).

## Quick start

### Build from source

```bash
# Linux build deps (fnox-core's hidapi → libudev1):
sudo apt-get install -y pkg-config libudev-dev

cargo build --release
# binary at ./target/release/remo-broker
```

Requires Rust **1.95** (pinned in `rust-toolchain.toml`).

### Run it manually (development)

```bash
# Provide a bootstrap token on disk:
mkdir -p /tmp/remo-broker
echo "your-bootstrap-token-here" > /tmp/remo-broker/bootstrap-token

# Provide a fnox.toml (see https://github.com/jdx/fnox for syntax):
cat > /tmp/remo-broker/fnox.toml <<'EOF'
# minimal example using the local-file provider
EOF

mkdir -p /tmp/remo-broker/run /tmp/remo-broker/log

./target/release/remo-broker \
  --bootstrap-token-path /tmp/remo-broker/bootstrap-token \
  --fnox-config          /tmp/remo-broker/fnox.toml \
  --socket-dir           /tmp/remo-broker/run \
  --audit-log-path       /tmp/remo-broker/log/audit.log
```

Talk to the admin socket:

```bash
echo '{"op":"status"}' | socat - UNIX-CONNECT:/tmp/remo-broker/run/admin.sock
```

Register a project and fetch a secret (assuming the project has a manifest
declaring `MY_SECRET` and fnox-core can resolve it):

```bash
# in a project directory with .remo/broker.toml or
# .devcontainer/remo-broker.toml:
echo '{"op":"register","name":"myproj","project_path":"'$PWD'"}' \
  | socat - UNIX-CONNECT:/tmp/remo-broker/run/admin.sock

echo '{"op":"get","name":"MY_SECRET"}' \
  | socat - UNIX-CONNECT:/tmp/remo-broker/run/myproj.sock
```

### Run it under systemd (production)

See [`packaging/README.md`](packaging/README.md) for the operator install:
unit file placement, sysusers / tmpfiles setup, bootstrap-token
provisioning (plaintext or TPM2-sealed via `LoadCredentialEncrypted=`),
fnox-core config, and troubleshooting.

## Project layout

```
src/
  main.rs        - thin CLI binary; wires Config → BackendSession → Server
  lib.rs         - module re-exports
  config.rs      - /etc/remo-broker/config.toml parsing + CLI overrides
  bootstrap.rs   - bootstrap-token resolver (file / env / IMDSv2)
  backend.rs     - fnox-core wrapper (Arc<ArcSwap<Fnox>> for rotate-bootstrap)
  manifest.rs    - per-project remo-broker.toml parser
  registry.rs    - ProjectRegistry + per-project state
  cache.rs       - BoundedCache (per-project, zeroize-on-drop)
  audit.rs       - JSONL audit log + async writer + degraded-mode buffer
  proto/         - wire-protocol request/response types
  server.rs      - daemon harness, admin + project socket loops, dispatch
docs/
  wire-protocol.md     - admin and project socket protocols
  manifest-schema.md   - .remo/broker.toml schema (v1)
packaging/
  systemd/             - remo-broker.service unit + hardening
  sysusers.d/          - remo-broker system user
  tmpfiles.d/          - /run/remo-broker, /var/log/remo-broker
  README.md            - operator install guide
specs/
  001-broker-daemon/   - the canonical spec
```

## Development

```bash
cargo build --all-targets --all-features
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check
```

CI runs all of the above on every push and PR, plus
`systemd-analyze verify` on the unit file. See
[`.github/workflows/ci.yml`](.github/workflows/ci.yml).

### Architecture notes

The spec's
[Key Implementation Decisions](specs/001-broker-daemon/spec.md#key-implementation-decisions)
table is the non-obvious-decisions index. Worth reading before changing
load-bearing modules. Examples of decisions captured there: why
`Project.manifest` is `ArcSwap<Manifest>`, why audit emission happens
before response serialization, why the IMDS client is hand-rolled,
why six RUSTSEC advisories are accepted with documented rationale.

### Testing conventions

- Tests live in `mod tests` inside the module under test (no separate
  `tests/` integration directory yet).
- Hand-rolled `TempDir` per module — no `tempfile` crate dep.
- Env-mutating tests use per-test unique variable names because
  `std::env::set_var` is `unsafe` in edition 2024.
- Wire-protocol response tests compare serialized JSON to a `json!`
  literal copied from `docs/wire-protocol.md`. Pin against silent
  serde drift.

## Security

- The audit log never contains values. The event types have no
  `value` field. Tests verify both the structural guarantee and a
  runtime grep with a planted tripwire value.
- Cached values are `secrecy::SecretString`; eviction / expiry /
  unregister / cache clear all zeroize.
- `deny.toml` documents six accepted RUSTSEC advisories from fnox-core's
  AWS-SDK / hyper / rustls dependency tree, each with a per-entry
  rationale. The list is mirrored into the rustsec/audit-check
  action's `ignore` input so `cargo audit` also stays green. Review
  on every dep bump.
- Report security issues by opening a [private security
  advisory on GitHub](https://github.com/get2knowio/remo-broker/security/advisories/new).

## License

MIT. See [`LICENSE`](LICENSE).
