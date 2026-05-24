# Binary size snapshot (NFR-005 follow-up)

`remo-broker`'s release binary is currently **~32 MiB**, versus an
NFR-005 target of **≤15 MiB**. This document records *where the bytes
go* so future reduction work has concrete targets instead of "AWS SDK
somewhere."

The numbers below are from `cargo bloat --release --crates -n 25`
against commit `d26a7e9` on Linux x86_64. To refresh, install
`cargo-bloat` (`cargo install cargo-bloat`) and re-run.

## Top 25 crates by `.text` contribution

| Rank | Crate | Size | % of `.text` | Notes |
|---|---|---:|---:|---|
| 1 | `std` | 3.6 MiB | 15.5% | Rust standard library. Unavoidable. |
| 2 | `openssl_sys` | 2.0 MiB | 8.5% | OpenSSL C bindings. Pulled by some fnox provider (likely Google Cloud / Azure). |
| 3 | `[Unknown]` (generics/monomorphization) | 1.8 MiB | 7.8% | Cross-crate generic code. Reducing trait/generic surface in our own crates would help marginally. |
| 4 | `aws_lc_sys` | 1.3 MiB | 5.5% | AWS-LC crypto library — separate from OpenSSL and rustls. Pulled by `aws-sdk-*`. |
| 5 | `h2` | 1.2 MiB | 5.4% | HTTP/2 implementation. Used by hyper for gRPC-style calls. |
| 6 | `fnox_core` | 939 KiB | 4.0% | The integration itself. Reasonable for a multi-backend resolver. |
| 7 | `google_cloud_secretmanager_v1` | 610 KiB | 2.6% | **Google Cloud SM provider, compiled in unconditionally even though no install uses it.** Highest single feature-flag win. |
| 8 | `rustls` | 597 KiB | 2.5% | Yet another TLS stack. AWS SDK uses both rustls and aws-lc. |
| 9 | `hyper` | 531 KiB | 2.3% | HTTP/1.1 client. Required by AWS SDK + reqwest. |
| 10 | `aws_smithy_runtime` | 456 KiB | 1.9% | AWS SDK runtime. |
| 11 | `serde` | 425 KiB | 1.8% | Used everywhere. Hard to remove. |
| 12 | `regex_automata` | 410 KiB | 1.8% | Likely pulled by tracing-subscriber's filter parsing + AWS SDK validators. |
| 13 | `google_cloud_auth` | 396 KiB | 1.7% | **Google Cloud auth, compiled in unconditionally.** Pairs with #7 as another feature-flag win. |
| 14 | `aws_config` | 377 KiB | 1.6% | AWS credential provider chain. |
| 15 | `hyper_util` | 341 KiB | 1.5% | Hyper utilities. |
| 16 | `tokio` | 313 KiB | 1.3% | Async runtime. We use it directly; unavoidable. |
| 17 | `ring` | 275 KiB | 1.2% | Yet another crypto library (third one). Pulled by rustls. |
| 18 | `aws_smithy_types` | 266 KiB | 1.1% | AWS SDK types. |
| 19 | `reqwest` | 241 KiB | 1.0% | Generic HTTP client. Probably pulled by a fnox provider. |
| 20 | **`remo_broker`** | 241 KiB | 1.0% | **Our own crate.** Right-sized. |
| 21 | `keepass` | 235 KiB | 1.0% | **KeePass provider, compiled in unconditionally.** Feature-flag win. |
| 22 | `aws_sdk_secretsmanager` | 221 KiB | 0.9% | The AWS Secrets Manager client. |
| 23 | `http` | 215 KiB | 0.9% | HTTP type definitions. |
| 24 | `clap_builder` | 214 KiB | 0.9% | CLI argument parsing. Could be trimmed by switching to derive-only mode or a lighter parser. |
| 25 | `serde_core` | 211 KiB | 0.9% | Serde core. |
| — | (227 more crates) | 5.3 MiB | 23.0% | Long tail. |
| **Total** | — | **22.9 MiB** `.text` / **41.4 MiB** file | — | Note: file size when built with debug symbols intact; the production-profile binary strips to ~32 MiB. |

## Reduction options, in priority order

### 1. Feature-gate providers in `fnox-core` (upstream change)

The single largest win: **rows 7 (`google_cloud_secretmanager_v1`,
610 KiB), 13 (`google_cloud_auth`, 396 KiB), and 21 (`keepass`,
235 KiB) are all providers compiled in unconditionally** even though
a given install only uses one or two of them. Adding Cargo feature
flags upstream would let us pick the providers we need:

```toml
fnox-core = { version = "1.25", default-features = false, features = ["aws", "vault"] }
```

Conservative estimate of savings if we drop Google + KeePass +
Azure-ish bits: **2-4 MiB**, plus their transitive deps (likely
another 2-3 MiB from `openssl_sys` if Google was its sole consumer).

**Action**: file an upstream issue at jdx/fnox requesting per-provider
feature flags. Track the upstream version that ships them.

### 2. Pick one TLS stack

We're linking **three crypto libraries**: `openssl_sys` (#2, 2.0 MiB),
`aws_lc_sys` (#4, 1.3 MiB), and `rustls` + `ring` (#8 + #17,
~870 KiB combined). The triple stack is because:

- The AWS SDK uses `aws_lc_sys` for SigV4 and `rustls` for TLS.
- Some Google Cloud / Azure-ish path pulls `openssl_sys`.
- `rustls` pulls `ring`.

If we feature-gate to a single TLS stack (rustls-only, dropping
openssl_sys + aws_lc_sys), savings **~3-3.5 MiB**. Depends on AWS
SDK supporting rustls-only builds — needs verification.

### 3. Profile / link-time optimizations

`Cargo.toml`'s release profile is already aggressive:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"
```

Options to push further:

- `lto = "fat"` instead of `"thin"`: typically saves 5-15%. Slower
  link times.
- `opt-level = "z"` instead of the default `"3"`: optimizes for size,
  ~10-20% savings, runtime cost variable. For a long-lived daemon
  with mostly I/O-bound work this is probably an acceptable trade.
- `strip = "debuginfo"` is the same as `strip = "symbols"` in current
  Cargo; no further gain available there.

Estimated combined savings: **2-5 MiB**.

### 4. Long tail (rows 24+)

Marginal. `clap`'s derive vs builder, `regex_automata` minimization,
trimming our own monomorphizations. Probably **<1 MiB** in total.

## Realistic 15 MiB target?

Adding savings: 2-4 MiB (feature flags) + 3-3.5 MiB (single TLS) +
2-5 MiB (LTO/opt-z) ≈ **7-12 MiB potential reduction**.

That brings us from ~32 MiB to **~20-25 MiB**, which is still
above 15 MiB. Closing the rest requires either:

- A musl static build (smaller std), or
- Splitting providers into separate plugin binaries (architectural
  change), or
- Revising NFR-005 to a more achievable target (e.g., ≤25 MiB).

The roadmap item ([spec roadmap #2](../specs/001-broker-daemon/spec.md#deferred-work-and-roadmap))
is "investigate and decide" rather than "commit to 15 MiB" because
this analysis suggests the target may need re-examination.

## How to refresh this snapshot

```bash
cargo install cargo-bloat       # one-time
cargo clean                     # ensure a fresh, optimized build
cargo bloat --release --crates -n 25
```

Re-run after any dep bump that touches fnox-core or any of the top-10
crates, and re-commit this file.

## Attribution: what an upstream `fnox-core` feature-flag PR would unlock

This section quantifies the upstream-ticket pitch. It's based on a
manual attribution of the top-100 crates against `cargo tree -i` walks
from each candidate-for-removal crate; numbers are conservative
(direct + clearly-attributable transitives only). The actual savings
in a real built binary would be *higher* than the sum below because
dead-code elimination during link-time optimization also drops
unreached generic monomorphizations (the `[Unknown]` row that
contributes 1.8 MiB to `.text` today).

### Scenario: "Minimal Remo install" — AWS + age + plain only

This is the leanest realistic profile: a Remo user who only needs
AWS Secrets Manager + age-encrypted local secrets + the plain
provider for dev. Everything else is feature-gated off.

Crates removable under this scenario (each verified via
`cargo tree -i <crate>` to be reachable only through dropped
providers):

| Category | Crates | Cumulative `.text` |
|---|---|---:|
| Google Cloud | `openssl_sys` (2.0 MiB; pulled by `reqwest@0.12` → `google-cloud-auth` and `typespec_client_core` → `azure_core`), `google_cloud_secretmanager_v1` (610 KiB), `google_cloud_auth` (396 KiB), `google_cloud_gax_internal` (103 KiB), `google_cloud_gax` (100 KiB), `google_cloud_iam_v1` (31 KiB), `tonic` (132 KiB) | **3.4 MiB** |
| Azure | `azure_core` (33 KiB), `azure_identity` (27 KiB), `typespec_client_core` (71 KiB), `quick_xml` (104 KiB; mostly Azure) | **235 KiB** |
| KeePass | `keepass` (235 KiB), `blake2b_simd` (22 KiB) | **257 KiB** |
| YubiKey / CTAP | `ctap_hid_fido2` (62 KiB), `hidapi` (in long tail; freed only on non-musl builds) | **~62 KiB** |
| D-Bus / Linux keyring | `dbus` (166 KiB), `libdbus_sys` (96 KiB), `dbus_secret_service` (54 KiB), `dbus_secret_service_keyring_store` (22 KiB) | **338 KiB** |
| JWT / RSA (used only by JWT-auth providers like GCP) | `jsonwebtoken` (57 KiB), `rsa` (35 KiB), `num_bigint_dig` (63 KiB), `num_bigint` (27 KiB) | **182 KiB** |
| `reqwest@0.12` (only Google + Azure use it; fnox-core itself uses `reqwest@0.13`) | half of the 241 KiB attributed to reqwest in the bloat snapshot, conservatively | **~120 KiB** |
| Other transitives reachable only via the dropped chain (long tail) | est. 30–50% of the 621 KiB "152 more crates" tail | **~250 KiB** |
| **Conservative direct + transitive total** | | **≈ 4.8 MiB of `.text`** |
| Generic monomorphizations no longer reached (fraction of `[Unknown]` 1.8 MiB) | est. 30% | **+ 540 KiB** |
| **Final conservative estimate** | | **≈ 5.4 MiB of `.text`** |

`.text` is currently 22.9 MiB; the stripped release binary is 32 MiB
(31.6 MiB measured via `stat`). Mapping back:

| | Before | After (minimal scenario) | Δ |
|---|---:|---:|---:|
| `.text` section | 22.9 MiB | ~17.5 MiB | **-5.4 MiB / -24%** |
| Stripped binary | 32 MiB | **~25–26 MiB** | **-6–7 MiB / -19–22%** |

So the upstream PR is worth, **conservatively, a 19–22% binary-size
reduction** for the minimal Remo profile. Users who keep one or two
additional providers (Vault, 1Password, etc.) save proportionally
less but still a meaningful chunk.

### PR feasibility

fnox-core's structure is well-suited to a feature-flag PR:

- Each provider is a self-contained file in
  `crates/fnox-core/src/providers/<name>.rs`.
- Each has a descriptor TOML in `crates/fnox-core/providers/*.toml`.
- A build script (`build/generate_providers.rs`) reads the
  descriptors and emits registration code into `providers/generated/`.

A clean PR would:

1. Add an `optional = true` flag (and group features) for each
   provider's deps in `Cargo.toml`.
2. Add a `[features]` block with one feature per provider; `default`
   includes everything (so existing users see no change).
3. Wrap each `pub mod foo;` declaration in `providers/mod.rs` with
   `#[cfg(feature = "foo")]`.
4. Teach `build/generate_providers.rs` to emit `#[cfg(feature = ...)]`
   guards around each generated provider entry.
5. Documentation + a `[features]` reference in the README.

Estimated effort: **6–8 hours** of focused work, plus review
iteration with the upstream maintainer. The change is mostly
additive and backward-compatible (all features default on), so the
review risk is low.
