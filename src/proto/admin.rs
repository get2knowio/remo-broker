//! Admin (control-plane) socket types. See `docs/wire-protocol.md` §3.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{ErrorResponse, PROTOCOL_VERSION};

/// All operations a privileged caller may send on the admin socket.
///
/// Tagged on `"op"` with kebab-case naming so multi-word ops match the doc
/// (`rotate-bootstrap`).
///
/// Unknown fields are intentionally tolerated per wire-protocol.md §4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum AdminRequest {
    Register { name: String, project_path: PathBuf },
    Unregister { name: String },
    Reload { name: String },
    Status,
    RotateBootstrap,
}

/// Error codes the broker may return on the admin socket.
/// See `docs/wire-protocol.md` §3 "Error codes (admin socket)".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminErrorCode {
    ManifestInvalid,
    ManifestNotFound,
    ProjectUnknown,
    ProjectExists,
    BootstrapError,
    ProtocolError,
    InternalError,
}

pub type AdminError = ErrorResponse<AdminErrorCode>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub ok: bool,
    pub socket_path: PathBuf,
}

impl RegisterResponse {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            ok: true,
            socket_path: socket_path.into(),
        }
    }
}

/// Bare-ok response shared by `unregister` (and any future op with no payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkResponse {
    pub ok: bool,
}

impl Default for OkResponse {
    fn default() -> Self {
        Self { ok: true }
    }
}

impl OkResponse {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadResponse {
    pub ok: bool,
    pub allowlist: Vec<String>,
}

impl ReloadResponse {
    pub fn new(allowlist: Vec<String>) -> Self {
        Self {
            ok: true,
            allowlist,
        }
    }
}

/// Bootstrap source as advertised by the daemon in `status` responses.
/// Wire form is kebab-case; matches the daemon's configuration vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootstrapMode {
    File,
    Imds,
    Env,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub ok: bool,
    pub broker_version: String,
    pub protocol_version: u32,
    pub uptime_seconds: u64,
    pub bootstrap_mode: BootstrapMode,
    pub projects: Vec<ProjectStatus>,
}

impl StatusResponse {
    pub fn new(
        broker_version: impl Into<String>,
        uptime_seconds: u64,
        bootstrap_mode: BootstrapMode,
        projects: Vec<ProjectStatus>,
    ) -> Self {
        Self {
            ok: true,
            broker_version: broker_version.into(),
            protocol_version: PROTOCOL_VERSION,
            uptime_seconds,
            bootstrap_mode,
            projects,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub name: String,
    pub socket_path: PathBuf,
    pub allowlist_size: u32,
    pub cache_entries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateBootstrapResponse {
    pub ok: bool,
    pub backend_auth: BackendAuthState,
}

/// State of the upstream backend session after a `rotate-bootstrap` attempt.
/// Currently only `ok` is emitted on success; failures use [`AdminError`] with
/// code `bootstrap_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendAuthState {
    Ok,
}

impl RotateBootstrapResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            backend_auth: BackendAuthState::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn roundtrip<T>(v: &T) -> Value
    where
        T: Serialize,
    {
        serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap()
    }

    // ---- requests ----

    #[test]
    fn parses_register_request() {
        let req: AdminRequest = serde_json::from_str(
            r#"{"op":"register","name":"myrepo","project_path":"/projects/myrepo"}"#,
        )
        .unwrap();
        assert_eq!(
            req,
            AdminRequest::Register {
                name: "myrepo".into(),
                project_path: PathBuf::from("/projects/myrepo"),
            }
        );
    }

    #[test]
    fn parses_unregister_request() {
        let req: AdminRequest =
            serde_json::from_str(r#"{"op":"unregister","name":"myrepo"}"#).unwrap();
        assert_eq!(
            req,
            AdminRequest::Unregister {
                name: "myrepo".into()
            }
        );
    }

    #[test]
    fn parses_reload_request() {
        let req: AdminRequest = serde_json::from_str(r#"{"op":"reload","name":"myrepo"}"#).unwrap();
        assert_eq!(
            req,
            AdminRequest::Reload {
                name: "myrepo".into()
            }
        );
    }

    #[test]
    fn parses_status_request() {
        let req: AdminRequest = serde_json::from_str(r#"{"op":"status"}"#).unwrap();
        assert_eq!(req, AdminRequest::Status);
    }

    #[test]
    fn parses_rotate_bootstrap_request_kebab_case() {
        let req: AdminRequest = serde_json::from_str(r#"{"op":"rotate-bootstrap"}"#).unwrap();
        assert_eq!(req, AdminRequest::RotateBootstrap);
    }

    #[test]
    fn rejects_unknown_admin_op() {
        let err = serde_json::from_str::<AdminRequest>(r#"{"op":"shutdown"}"#).unwrap_err();
        assert!(err.to_string().contains("shutdown") || err.to_string().contains("variant"));
    }

    // ---- responses match the wire-protocol doc verbatim ----

    #[test]
    fn register_response_matches_doc_example() {
        let r = RegisterResponse::new("/run/remo-broker/myrepo.sock");
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "socket_path": "/run/remo-broker/myrepo.sock",
            })
        );
    }

    #[test]
    fn unregister_response_matches_doc_example() {
        let r = OkResponse::new();
        assert_eq!(roundtrip(&r), json!({"ok": true}));
    }

    #[test]
    fn reload_response_matches_doc_example() {
        let r = ReloadResponse::new(vec![
            "GITHUB_TOKEN".into(),
            "NPM_TOKEN".into(),
            "ANTHROPIC_API_KEY".into(),
        ]);
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN", "ANTHROPIC_API_KEY"],
            })
        );
    }

    #[test]
    fn status_response_matches_doc_example() {
        let r = StatusResponse::new(
            "0.3.1",
            84273,
            BootstrapMode::Imds,
            vec![
                ProjectStatus {
                    name: "myrepo".into(),
                    socket_path: PathBuf::from("/run/remo-broker/myrepo.sock"),
                    allowlist_size: 3,
                    cache_entries: 2,
                },
                ProjectStatus {
                    name: "other".into(),
                    socket_path: PathBuf::from("/run/remo-broker/other.sock"),
                    allowlist_size: 1,
                    cache_entries: 0,
                },
            ],
        );
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "broker_version": "0.3.1",
                "protocol_version": 1,
                "uptime_seconds": 84273,
                "bootstrap_mode": "imds",
                "projects": [
                    {"name": "myrepo", "socket_path": "/run/remo-broker/myrepo.sock", "allowlist_size": 3, "cache_entries": 2},
                    {"name": "other",  "socket_path": "/run/remo-broker/other.sock",  "allowlist_size": 1, "cache_entries": 0},
                ],
            })
        );
    }

    #[test]
    fn rotate_bootstrap_response_matches_doc_example() {
        let r = RotateBootstrapResponse::ok();
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "backend_auth": "ok",
            })
        );
    }

    // ---- error responses ----

    #[test]
    fn manifest_invalid_error_matches_doc_example() {
        let e = AdminError::new(AdminErrorCode::ManifestInvalid, "details here");
        assert_eq!(
            roundtrip(&e),
            json!({
                "ok": false,
                "code": "manifest_invalid",
                "message": "details here",
            })
        );
    }

    #[test]
    fn all_admin_error_codes_round_trip() {
        use AdminErrorCode::*;
        let expected = [
            (ManifestInvalid, "manifest_invalid"),
            (ManifestNotFound, "manifest_not_found"),
            (ProjectUnknown, "project_unknown"),
            (ProjectExists, "project_exists"),
            (BootstrapError, "bootstrap_error"),
            (ProtocolError, "protocol_error"),
            (InternalError, "internal_error"),
        ];
        for (code, wire) in expected {
            let s = serde_json::to_value(code).unwrap();
            assert_eq!(s, Value::String(wire.to_string()), "variant {code:?}");
            let back: AdminErrorCode = serde_json::from_value(s).unwrap();
            assert_eq!(back, code);
        }
    }
}
