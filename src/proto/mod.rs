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
