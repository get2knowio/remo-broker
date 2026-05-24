//! Bootstrap-token resolution.
//!
//! Turns a [`BootstrapSource`](crate::config::BootstrapSource) into an
//! in-memory [`BootstrapToken`] that the upstream fnox-core session can later
//! authenticate with. Implements the runtime half of FR-002 and FR-003.
//!
//! IMDSv2 support (FR-002b) is deferred — it needs an HTTP client and a
//! separate test plan against the EC2 metadata service. The variant errors
//! with [`BootstrapError::ImdsNotImplemented`] today so callers can still
//! exercise the rest of the startup path on a developer laptop.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

use crate::config::{BOOTSTRAP_ENV_VAR, BootstrapSource};

/// An opaque, zeroize-on-drop bootstrap token.
///
/// Wraps `secrecy::SecretString` so misuse — printing via `Debug`, accidental
/// serialization — is a compile-time concern. The placeholder type lets us
/// swap to `fnox_core::SecretBox` as a single type alias once that crate
/// lands.
#[derive(Clone)]
pub struct BootstrapToken(SecretString);

impl BootstrapToken {
    /// Construct from a string. Caller is responsible for any trimming.
    fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    /// Borrow the underlying string. Use sparingly — every additional call
    /// site is one more place a leak could be introduced.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl std::fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BootstrapToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("bootstrap token file {path} not found")]
    FileNotFound { path: PathBuf },

    #[error("bootstrap token file {path} is empty")]
    FileEmpty { path: PathBuf },

    #[error("failed to read bootstrap token file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("env var {var} is not set")]
    EnvUnset { var: String },

    #[error("env var {var} is set but empty")]
    EnvEmpty { var: String },

    #[error("imds bootstrap source is not yet implemented (FR-002b deferred)")]
    ImdsNotImplemented,
}

/// Resolve the bootstrap source into a usable token.
///
/// Async even for file/env so the signature does not change when IMDSv2 (an
/// HTTP call) lands.
pub async fn fetch_token(source: &BootstrapSource) -> Result<BootstrapToken, BootstrapError> {
    match source {
        BootstrapSource::File { path } => fetch_file(path).await,
        BootstrapSource::Env => fetch_env_with_var(BOOTSTRAP_ENV_VAR),
        BootstrapSource::Imds => Err(BootstrapError::ImdsNotImplemented),
    }
}

async fn fetch_file(path: &Path) -> Result<BootstrapToken, BootstrapError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(BootstrapError::FileEmpty {
                    path: path.to_path_buf(),
                });
            }
            Ok(BootstrapToken::new(trimmed.to_owned()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(BootstrapError::FileNotFound {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(BootstrapError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn fetch_env_with_var(var: &str) -> Result<BootstrapToken, BootstrapError> {
    match std::env::var(var) {
        Ok(s) if s.is_empty() => Err(BootstrapError::EnvEmpty {
            var: var.to_owned(),
        }),
        Ok(s) => Ok(BootstrapToken::new(s)),
        Err(std::env::VarError::NotPresent) => Err(BootstrapError::EnvUnset {
            var: var.to_owned(),
        }),
        Err(std::env::VarError::NotUnicode(_)) => Err(BootstrapError::EnvEmpty {
            var: var.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---- file source ----

    #[tokio::test]
    async fn file_present_returns_trimmed_token() {
        let dir = tempdir();
        let path = dir.path().join("token");
        std::fs::write(&path, "  hello-token\n\n").unwrap();
        let t = fetch_token(&BootstrapSource::File { path }).await.unwrap();
        assert_eq!(t.expose(), "hello-token");
    }

    #[tokio::test]
    async fn file_missing_errors_with_file_not_found() {
        let path = nonexistent_path("missing-token");
        let err = fetch_token(&BootstrapSource::File { path: path.clone() })
            .await
            .unwrap_err();
        match err {
            BootstrapError::FileNotFound { path: p } => assert_eq!(p, path),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_empty_errors_with_file_empty() {
        let dir = tempdir();
        let path = dir.path().join("token");
        std::fs::write(&path, "   \n\t\n").unwrap();
        let err = fetch_token(&BootstrapSource::File { path: path.clone() })
            .await
            .unwrap_err();
        assert!(matches!(err, BootstrapError::FileEmpty { .. }));
    }

    // ---- env source ----
    //
    // Env vars are process-global state; tests use unique var names per test
    // so they cannot collide with each other or with a real
    // `REMO_BROKER_BOOTSTRAP_TOKEN`. `std::env::set_var` is `unsafe` in the
    // 2024 edition because concurrent reads from other threads are UB; we
    // accept that risk here — these tests don't spawn additional threads
    // that touch env after the set_var call.

    #[test]
    fn env_present_returns_token() {
        let var = unique_env_var("present");
        unsafe { std::env::set_var(&var, "env-token-value") };
        let result = fetch_env_with_var(&var);
        unsafe { std::env::remove_var(&var) };
        let t = result.unwrap();
        assert_eq!(t.expose(), "env-token-value");
    }

    #[test]
    fn env_unset_errors_with_env_unset() {
        let var = unique_env_var("unset");
        // Belt-and-braces: clear in case a previous run leaked.
        unsafe { std::env::remove_var(&var) };
        let err = fetch_env_with_var(&var).unwrap_err();
        match err {
            BootstrapError::EnvUnset { var: v } => assert_eq!(v, var),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn env_empty_errors_with_env_empty() {
        let var = unique_env_var("empty");
        unsafe { std::env::set_var(&var, "") };
        let result = fetch_env_with_var(&var);
        unsafe { std::env::remove_var(&var) };
        let err = result.unwrap_err();
        match err {
            BootstrapError::EnvEmpty { var: v } => assert_eq!(v, var),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- imds source ----

    #[tokio::test]
    async fn imds_returns_not_implemented_today() {
        // FR-002b is deferred. When IMDSv2 lands this test should flip to
        // exercise the happy path (likely behind a feature flag or via a
        // mocked metadata endpoint).
        let err = fetch_token(&BootstrapSource::Imds).await.unwrap_err();
        assert!(matches!(err, BootstrapError::ImdsNotImplemented));
    }

    // ---- redaction ----

    #[test]
    fn debug_output_does_not_leak_value() {
        let t = BootstrapToken::new("super-secret-value-xyz".into());
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("super-secret-value-xyz"), "leaked: {dbg}");
        assert!(dbg.contains("REDACTED"), "no redaction marker: {dbg}");
    }

    // ---- helpers ----

    fn unique_env_var(label: &str) -> String {
        format!(
            "REMO_BROKER_TEST_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn nonexistent_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "remo-broker-test-{label}-{}-{}-missing",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn tempdir() -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "remo-broker-test-bootstrap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
