# `remo-broker.toml` Manifest Schema

**Status**: Draft — schema_version 1
**Audience**: Remo (writer), `remo-broker` (reader), project authors (manual editors)
**Source of truth**: this document; the Rust types in `src/manifest.rs` MUST conform; the JSON Schema artifact (`schema/remo-broker.v1.json`) is generated from the Rust types and published per release.

## Purpose

The manifest declares, per project, the set of backend-resolvable secret *names* that project's devcontainer is permitted to fetch via the broker. The broker reads the manifest when creating the project socket and uses it as the per-project allowlist. Backend-side mappings (which credential store each name lives in, which backend identity to use) live separately in the instance-level fnox-core configuration and are not addressable from the manifest.

## File locations

Discovered by the broker, in this priority order:

1. `<project_path>/.devcontainer/remo-broker.toml` — committed to the repo, authored by the project owner.
2. `<project_path>/.remo/broker.toml` — auto-synthesized by Remo when no committed manifest exists. Should be in `.gitignore` (Remo adds it).

If neither file exists, the broker MUST refuse to create a project socket and emit a clear error in its audit log.

## Schema (v1)

```toml
schema_version = 1

[project]
name = "my-project"           # required; matches the project directory basename
description = "..."           # optional; free-form, surfaced in audit log and `remo-broker status`

[allowlist]
secrets = [                   # required; list of allowed secret names (case-sensitive)
  "GITHUB_TOKEN",
  "NPM_TOKEN",
  "ANTHROPIC_API_KEY",
]

[cache]                       # optional; per-project overrides of broker defaults
ttl_seconds = 3600            # default: broker-wide default (typically 900)
max_entries = 64              # default: broker-wide default (typically 32)
```

## Field reference

### Top-level

| Key | Type | Required | Notes |
|---|---|---|---|
| `schema_version` | integer | yes | Currently `1`. Broker MUST refuse unknown versions with a clear error. |

### `[project]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Lowercase ASCII alphanumeric, `-`, `_`; 1–64 chars; matches project directory basename. Used in the project-socket filename. |
| `description` | string | no | ≤256 chars; informational only. |

### `[allowlist]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `secrets` | array&lt;string&gt; | yes (may be empty) | Each entry: 1–128 chars, ASCII alphanumeric + `_`. Duplicates are an error. Case-sensitive. |

An empty `secrets = []` is legal — produces a project socket that denies every fetch and exists only to make the project's presence in the broker explicit (useful for "we know about this project but it doesn't need any secrets yet").

### `[cache]`

| Key | Type | Required | Notes |
|---|---|---|---|
| `ttl_seconds` | integer | no | 1–86400. Caps the broker-wide default downward (never raises it). |
| `max_entries` | integer | no | 1–1024. Caps the broker-wide default downward. |

A project cannot raise cache limits above the broker-wide defaults — only lower them. This prevents a permissive project manifest from increasing exposure-window for a leaked secret.

## Validation rules

The broker MUST enforce all of the following at manifest load time. Any failure causes the project socket to not be created and an audit-log entry to be written with `event = "manifest.invalid"`.

1. File is valid TOML 1.0.
2. `schema_version` is present and equals a version the broker supports.
3. `[project].name` matches `^[a-z0-9][a-z0-9_-]{0,63}$`.
4. `[project].name` matches the project directory basename. (Defense against a hand-edited manifest claiming a different identity.)
5. `[allowlist].secrets` exists and is an array of strings; each string matches `^[A-Za-z0-9_]{1,128}$`; no duplicates.
6. `[cache]` numeric fields are within their allowed ranges and do not exceed broker-wide defaults.
7. No unknown top-level tables and no unknown keys within known tables. (Strict mode: typos fail loudly. May relax to warning-only behind a flag in a future version.)

## Compatibility commitments

The broker's manifest support follows these rules:

- **Within `schema_version = 1`**: only additive, non-breaking changes. New optional keys may appear; existing keys' semantics and types do not change.
- **`schema_version = 2` and above**: may introduce breaking changes. Both the previous and new schema versions are accepted by the broker for a transition window of at least one minor release.
- **JSON Schema artifact**: `schema/remo-broker.v1.json` ships with every broker release. Remo (Python) validates manifests against this artifact at `remo init` and at devcontainer launch. The artifact is content-addressable and versioned; Remo pins to a specific broker minor version's schema.
- **Strict-mode unknown-key handling**: a future schema_version may introduce `[experimental]` tables intentionally outside the strict-mode check. Until then, unknown keys are errors.

## Example: minimal (auto-synthesized by Remo)

```toml
schema_version = 1

[project]
name = "myrepo"

[allowlist]
secrets = ["GITHUB_TOKEN"]
```

## Example: committed by the project owner

```toml
schema_version = 1

[project]
name = "internal-tool"
description = "Backend service; needs GitHub for git push, npm for publish, and Anthropic for the LLM features."

[allowlist]
secrets = [
  "GITHUB_TOKEN",
  "NPM_TOKEN",
  "ANTHROPIC_API_KEY",
]

[cache]
ttl_seconds = 600       # tighter than broker default
```

## Open questions

- **OQ-M1**: Should we support per-secret TTL overrides (e.g., shorter TTL for high-blast-radius secrets like `AWS_SECRET_ACCESS_KEY`)? Currently TTL is per-project.
- **OQ-M2**: Should manifests support inheriting an allowlist from a parent file (e.g., a workspace-level base allowlist that per-project manifests extend)? Adds complexity; unclear demand.
- **OQ-M3**: Should an `[audit]` section let projects request additional logging (e.g., "log every fetch of `AWS_SECRET_ACCESS_KEY` to a separate file") or is the broker-wide audit log sufficient?
