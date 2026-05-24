//! Backend session — thin wrapper over `fnox_core::Fnox`.
//!
//! Implements:
//! - **FR-004**: all backend retrieval goes through fnox-core. This module
//!   is the single place that names `fnox_core::*`; the rest of the broker
//!   talks to it through [`BackendSession`].
//! - **FR-005**: one `Fnox` instance is constructed at startup (and again
//!   on `rotate-bootstrap`), then shared across every project handler via
//!   `Arc<ArcSwap<Fnox>>`. The atomic swap means rotation never drops or
//!   serializes in-flight `get` calls.
//!
//! `Fnox::get` is async and returns `Result<Option<String>>`:
//! - `Ok(Some(value))` → cache + return.
//! - `Ok(None)` → secret was declared in fnox config but absent
//!   (`if_missing = "ignore"` or `"warn"`) → map to
//!   `ProjectErrorCode::NotFound`.
//! - `Err(e)` → provider/auth/transport failure →
//!   `ProjectErrorCode::BackendError`.
//!
//! We deliberately don't try to distinguish `backend_unreachable` from
//! `backend_error` here — fnox-core's error type doesn't carry the
//! distinction. Operators read the embedded message; the wire code stays
//! `backend_error`.

use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use fnox_core::library::Fnox;

/// Wraps an `ArcSwap<Fnox>` so handlers can fetch (`load_full`) without
/// blocking the rotate-bootstrap admin op (`store`). Cloning is a single
/// `Arc::clone`; share it freely across tasks.
#[derive(Clone)]
pub struct BackendSession {
    inner: Arc<ArcSwap<Fnox>>,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("failed to open fnox config at {path}: {source}")]
    Open {
        path: std::path::PathBuf,
        #[source]
        source: anyhow::Error,
    },

    #[error("failed to discover fnox config from current directory: {0}")]
    Discover(#[source] anyhow::Error),
}

impl BackendSession {
    /// Build a session from `fnox.toml` at `path` (no upward search, no
    /// merging). Use this when the operator has named a specific file.
    pub fn open(path: &Path) -> Result<Self, BackendError> {
        let fnox = Fnox::open(path).map_err(|e| BackendError::Open {
            path: path.to_path_buf(),
            source: anyhow::Error::from(e),
        })?;
        Ok(Self::from_fnox(fnox))
    }

    /// Build a session by walking upward from cwd, merging the
    /// parent/local/global config chain — same behavior as the `fnox` CLI.
    pub fn discover() -> Result<Self, BackendError> {
        let fnox = Fnox::discover().map_err(|e| BackendError::Discover(anyhow::Error::from(e)))?;
        Ok(Self::from_fnox(fnox))
    }

    fn from_fnox(fnox: Fnox) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(fnox)),
        }
    }

    /// Atomically replace the active session. Used by `rotate-bootstrap`
    /// once the new session has been successfully constructed — never
    /// install a half-built one.
    pub fn replace(&self, new: Fnox) {
        self.inner.store(Arc::new(new));
    }

    /// Atomically swap our underlying `Fnox` for the one held by `other`.
    /// Equivalent to `replace(other.load())` but avoids cloning the Fnox.
    /// Used by `rotate-bootstrap` after successfully constructing a fresh
    /// session — the throw-away `BackendSession` wrapper costs one `Arc`
    /// allocation and is dropped on the next line.
    pub fn adopt(&self, other: &Self) {
        self.inner.store(other.inner.load_full());
    }

    /// Lookup a secret by name. Returns:
    /// - `Ok(Some(value))` on success,
    /// - `Ok(None)` if fnox-core resolves the name to a missing-allowed slot,
    /// - `Err(message)` on any backend / transport / auth failure (the
    ///   string is suitable for embedding in a `backend_error` response).
    pub async fn get(&self, name: &str) -> Result<Option<String>, String> {
        let fnox = self.inner.load_full();
        // Spawning is unnecessary — `Fnox::get` is already async. The
        // clone is the cheap-clone Fnox documents.
        fnox.get(name).await.map_err(|e| e.to_string())
    }
}

impl std::fmt::Debug for BackendSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't print the underlying Fnox — its Debug isn't part of the
        // stable API and may include config-derived strings. A pointer
        // and a tag are enough for log triage.
        f.debug_struct("BackendSession")
            .field("inner", &Arc::as_ptr(&self.inner))
            .finish()
    }
}
