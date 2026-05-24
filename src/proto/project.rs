//! Project (data-plane) socket types. See `docs/wire-protocol.md` §2.

use serde::{Deserialize, Serialize};

use super::{ErrorResponse, PROTOCOL_VERSION};

/// All operations a devcontainer-side client may send on a project socket.
///
/// Tagged on `"op"` with kebab-case naming; the protocol doc shows lowercase
/// single-word ops (`get`, `ping`, `info`), all of which kebab-case leaves
/// untouched.
///
/// Unknown fields are intentionally tolerated per wire-protocol.md §4 (a v1
/// broker must accept additive request fields from v1.x clients).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum ProjectRequest {
    Get { name: String },
    Ping,
    Info,
}

/// Error codes the broker may return on the project socket.
/// See `docs/wire-protocol.md` §2 "Error codes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectErrorCode {
    Denied,
    NotFound,
    BackendError,
    BackendUnreachable,
    RateLimited,
    ProtocolError,
    InternalError,
    PeerUnexpected,
}

pub type ProjectError = ErrorResponse<ProjectErrorCode>;

/// Successful `get` response. Exactly one of `value` or `value_b64` is set:
/// `value` for UTF-8 values, `value_b64` for binary values per protocol §1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetResponse {
    /// Always `true`. Set by [`GetResponse::utf8`] / [`GetResponse::binary`].
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_b64: Option<String>,
    pub ttl_seconds: u32,
}

impl GetResponse {
    pub fn utf8(value: impl Into<String>, ttl_seconds: u32) -> Self {
        Self {
            ok: true,
            value: Some(value.into()),
            value_b64: None,
            ttl_seconds,
        }
    }

    pub fn binary(value_b64: impl Into<String>, ttl_seconds: u32) -> Self {
        Self {
            ok: true,
            value: None,
            value_b64: Some(value_b64.into()),
            ttl_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingResponse {
    /// Always `true`.
    pub ok: bool,
    pub broker_version: String,
    pub protocol_version: u32,
    pub project: String,
}

impl PingResponse {
    pub fn new(broker_version: impl Into<String>, project: impl Into<String>) -> Self {
        Self {
            ok: true,
            broker_version: broker_version.into(),
            protocol_version: PROTOCOL_VERSION,
            project: project.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InfoResponse {
    /// Always `true`.
    pub ok: bool,
    pub project: String,
    pub allowlist: Vec<String>,
    pub schema_version: u32,
}

impl InfoResponse {
    pub fn new(project: impl Into<String>, allowlist: Vec<String>, schema_version: u32) -> Self {
        Self {
            ok: true,
            project: project.into(),
            allowlist,
            schema_version,
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
    fn parses_get_request() {
        let req: ProjectRequest =
            serde_json::from_str(r#"{"op":"get","name":"GITHUB_TOKEN"}"#).unwrap();
        assert_eq!(
            req,
            ProjectRequest::Get {
                name: "GITHUB_TOKEN".into()
            }
        );
    }

    #[test]
    fn parses_ping_request() {
        let req: ProjectRequest = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert_eq!(req, ProjectRequest::Ping);
    }

    #[test]
    fn parses_info_request() {
        let req: ProjectRequest = serde_json::from_str(r#"{"op":"info"}"#).unwrap();
        assert_eq!(req, ProjectRequest::Info);
    }

    #[test]
    fn rejects_unknown_op() {
        let err = serde_json::from_str::<ProjectRequest>(r#"{"op":"nope"}"#).unwrap_err();
        assert!(err.to_string().contains("nope") || err.to_string().contains("variant"));
    }

    #[test]
    fn tolerates_unknown_field_on_request() {
        // Per wire-protocol.md §4, additive changes are allowed within a
        // major protocol version: new optional request fields. A v1 broker
        // must therefore tolerate unknown fields a v1.x client may send.
        let req: ProjectRequest = serde_json::from_str(r#"{"op":"ping","extra":1}"#).unwrap();
        assert_eq!(req, ProjectRequest::Ping);
    }

    // ---- responses match the wire-protocol doc verbatim ----

    #[test]
    fn get_utf8_response_matches_doc_example() {
        // From docs/wire-protocol.md §2 "get — fetch a secret value"
        let r = GetResponse::utf8("ghp_xxxxxxxxxxxxxxxxxxxx", 542);
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "value": "ghp_xxxxxxxxxxxxxxxxxxxx",
                "ttl_seconds": 542,
            })
        );
    }

    #[test]
    fn get_binary_response_matches_doc_example() {
        let r = GetResponse::binary("ZGVhZGJlZWY=", 542);
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "value_b64": "ZGVhZGJlZWY=",
                "ttl_seconds": 542,
            })
        );
    }

    #[test]
    fn ping_response_matches_doc_example() {
        let r = PingResponse::new("0.3.1", "myrepo");
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "broker_version": "0.3.1",
                "protocol_version": 1,
                "project": "myrepo",
            })
        );
    }

    #[test]
    fn info_response_matches_doc_example() {
        let r = InfoResponse::new("myrepo", vec!["GITHUB_TOKEN".into(), "NPM_TOKEN".into()], 1);
        assert_eq!(
            roundtrip(&r),
            json!({
                "ok": true,
                "project": "myrepo",
                "allowlist": ["GITHUB_TOKEN", "NPM_TOKEN"],
                "schema_version": 1,
            })
        );
    }

    // ---- error responses ----

    #[test]
    fn denied_error_matches_doc_example() {
        let e = ProjectError::new(
            ProjectErrorCode::Denied,
            "Secret 'NPM_TOKEN' is not in this project's allowlist.",
        );
        assert_eq!(
            roundtrip(&e),
            json!({
                "ok": false,
                "code": "denied",
                "message": "Secret 'NPM_TOKEN' is not in this project's allowlist.",
            })
        );
    }

    #[test]
    fn rate_limited_error_includes_retry_after() {
        let e = ProjectError::new(ProjectErrorCode::RateLimited, "Too many fetches in window.")
            .with_retry_after(10);
        assert_eq!(
            roundtrip(&e),
            json!({
                "ok": false,
                "code": "rate_limited",
                "message": "Too many fetches in window.",
                "retry_after_seconds": 10,
            })
        );
    }

    #[test]
    fn all_error_codes_round_trip() {
        // Defends against silent rename: if the wire code for any variant
        // changes, this test fires.
        use ProjectErrorCode::*;
        let expected = [
            (Denied, "denied"),
            (NotFound, "not_found"),
            (BackendError, "backend_error"),
            (BackendUnreachable, "backend_unreachable"),
            (RateLimited, "rate_limited"),
            (ProtocolError, "protocol_error"),
            (InternalError, "internal_error"),
            (PeerUnexpected, "peer_unexpected"),
        ];
        for (code, wire) in expected {
            let s = serde_json::to_value(code).unwrap();
            assert_eq!(s, Value::String(wire.to_string()), "variant {code:?}");
            let back: ProjectErrorCode = serde_json::from_value(s).unwrap();
            assert_eq!(back, code);
        }
    }
}
