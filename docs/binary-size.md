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
