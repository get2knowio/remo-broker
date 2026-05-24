//! Wire-protocol types for both broker sockets.
//!
//! Schema and semantics live in `docs/wire-protocol.md`; these types are the
//! source-of-truth Rust implementation it points to. Both sockets share the
//! NDJSON framing, max-message-size limit, and a common error-response shape;
//! the per-socket request/response shapes are in the submodules.

pub mod admin;
pub mod project;

use serde::{Deserialize, Serialize};

/// Wire protocol major version this crate speaks. Advertised in `ping` and
/// `status` responses. See `docs/wire-protocol.md` §4.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum size of a single request or response message, in bytes.
/// Per `docs/wire-protocol.md` §1. Frames over this limit are rejected with
/// a `protocol_error` and the connection is closed.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Error response shape shared by both sockets.
///
/// Generic over the code type so each socket can constrain its own error
/// codes via [`project::ProjectErrorCode`] / [`admin::AdminErrorCode`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse<Code> {
    /// Always `false`. Set by [`ErrorResponse::new`].
    pub ok: bool,
    pub code: Code,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u32>,
}

impl<Code> ErrorResponse<Code> {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            code,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn with_retry_after(mut self, seconds: u32) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }
}

// -------------------------------------------------------------------------
// Smoke fuzz — partial SC-001
// -------------------------------------------------------------------------
//
// The full SC-001 requirement is a 24-hour cargo-fuzz campaign with zero
// panics and zero memory growth; that needs nightly Rust + a separate
// scheduled CI job. The tests below are the cheap, every-PR version: they
// feed deterministically-seeded random byte sequences through each
// public request type's serde deserialization and assert no panic. Any
// `Result` is fine (success or `Err`); panics are not.
//
// 10 000 iterations per parser × 256-byte payloads, capped at the wire
// limit. Runs in well under a second.

#[cfg(test)]
mod smoke_fuzz {
    use super::admin::AdminRequest;
    use super::project::ProjectRequest;

    /// Tiny SplitMix64-style PRNG, deterministic and dependency-free.
    /// We don't need cryptographic quality — just a reproducible stream.
    struct SplitMix64(u64);
    impl SplitMix64 {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let bytes = self.next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
        }
    }

    fn fuzz_parser<T>(seed: u64, iterations: usize, max_len: usize)
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let mut rng = SplitMix64(seed);
        let mut buf = vec![0u8; max_len];
        for i in 0..iterations {
            // Vary the payload length each iteration so we hit both short
            // and full-frame inputs. Keep within MAX_MESSAGE_BYTES.
            let len = (rng.next_u64() as usize % max_len) + 1;
            let slice = &mut buf[..len];
            rng.fill(slice);
            // The only assertion: no panic. Both Ok and Err are fine.
            let result = serde_json::from_slice::<T>(slice);
            // Read the result so the compiler can't optimize the call away.
            let _ = std::hint::black_box(&result);
            // If we ever observe a panic-like result (which would be a
            // crash inside serde or our types), the iteration tells the
            // operator where to start.
            std::hint::black_box(i);
        }
    }

    #[test]
    fn project_request_random_bytes_never_panic() {
        // 10k iterations × up to 4 KiB payloads. Trimmed below 64 KiB
        // (the wire limit) so the test runs in well under a second;
        // larger frames are guarded by `read_line_capped` at the I/O
        // layer rather than serde.
        fuzz_parser::<ProjectRequest>(0x000A_11CE_FEED_BEEF, 10_000, 4096);
    }

    #[test]
    fn admin_request_random_bytes_never_panic() {
        fuzz_parser::<AdminRequest>(0x0000_BADD_C0DE_DEAD, 10_000, 4096);
    }

    /// Slightly different angle: bias the input toward "looks like JSON"
    /// (always start with `{`) so the parser gets further before
    /// rejecting. Catches panics that only fire after type-resolution
    /// has begun.
    #[test]
    fn project_request_json_shaped_bytes_never_panic() {
        let mut rng = SplitMix64(0xC0FFEE);
        let mut buf = vec![0u8; 2048];
        for _ in 0..10_000 {
            let len = (rng.next_u64() as usize % buf.len()) + 1;
            let slice = &mut buf[..len];
            rng.fill(slice);
            slice[0] = b'{';
            let _ = serde_json::from_slice::<ProjectRequest>(slice);
        }
    }

    #[test]
    fn admin_request_json_shaped_bytes_never_panic() {
        let mut rng = SplitMix64(0xDECAFBAD);
        let mut buf = vec![0u8; 2048];
        for _ in 0..10_000 {
            let len = (rng.next_u64() as usize % buf.len()) + 1;
            let slice = &mut buf[..len];
            rng.fill(slice);
            slice[0] = b'{';
            let _ = serde_json::from_slice::<AdminRequest>(slice);
        }
    }
}
