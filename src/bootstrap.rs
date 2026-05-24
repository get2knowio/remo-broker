//! Bootstrap-token resolution.
//!
//! Turns a [`BootstrapSource`](crate::config::BootstrapSource) into an
//! in-memory [`BootstrapToken`] that the upstream fnox-core session can later
//! authenticate with. Implements the runtime half of FR-002 / FR-002b /
//! FR-003.
//!
//! The IMDSv2 path hand-rolls a tiny HTTP/1.1 client against
//! `169.254.169.254` (PUT token → GET role → GET credentials). The full
//! exchange is three HTTP requests against a plain-HTTP, link-local
//! endpoint; pulling in a 500 KB+ HTTP client crate for that would be
//! disproportionate. The hand-rolled bits live in this module and are
//! exercised by an in-process mock server in tests.

use std::path::{Path, PathBuf};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::{BOOTSTRAP_ENV_VAR, BootstrapSource};

const IMDS_BASE_URL: &str = "http://169.254.169.254";
/// 6 h — AWS IMDS's maximum TTL. Per OQ-2 we do not refresh on our own;
/// fnox-core handles AWS credential rotation internally.
const IMDS_TOKEN_TTL_SECONDS: u32 = 21_600;
/// Per-call upper bound. The metadata service is link-local and usually
/// answers in milliseconds; anything longer points to a misrouted IMDS
/// query (running off-EC2) and we want to fail fast rather than wedge the
/// daemon's startup.
const IMDS_TIMEOUT: Duration = Duration::from_secs(2);

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

    #[error("imds bootstrap: invalid base url {0:?}")]
    ImdsInvalidBaseUrl(String),

    #[error("imds bootstrap: failed to connect to {addr}: {source}")]
    ImdsConnect {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("imds bootstrap: i/o error: {0}")]
    ImdsIo(#[source] std::io::Error),

    #[error("imds bootstrap: request to {endpoint} timed out after {timeout_ms} ms")]
    ImdsTimeout { endpoint: String, timeout_ms: u128 },

    #[error("imds bootstrap: {endpoint} returned HTTP {status}: {body}")]
    ImdsHttp {
        endpoint: String,
        status: u16,
        body: String,
    },

    #[error("imds bootstrap: malformed HTTP response from {endpoint}")]
    ImdsMalformedResponse { endpoint: String },

    #[error("imds bootstrap: metadata token endpoint returned empty body")]
    ImdsEmptyToken,

    #[error("imds bootstrap: no IAM instance role attached to this instance")]
    ImdsNoInstanceRole,
}

/// Resolve the bootstrap source into a usable token.
///
/// Async even for file/env so the signature does not change when IMDSv2 (an
/// HTTP call) lands.
pub async fn fetch_token(source: &BootstrapSource) -> Result<BootstrapToken, BootstrapError> {
    match source {
        BootstrapSource::File { path } => fetch_file(path).await,
        BootstrapSource::Env => fetch_env_with_var(BOOTSTRAP_ENV_VAR),
        BootstrapSource::Imds => fetch_imds_at(IMDS_BASE_URL, IMDS_TIMEOUT).await,
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

// -------------------------------------------------------------------------
// IMDSv2
// -------------------------------------------------------------------------

/// Orchestrate the three IMDSv2 calls and wrap the credentials JSON in a
/// `BootstrapToken`. `base_url` is parameterized for testability — in
/// production it's `IMDS_BASE_URL`.
async fn fetch_imds_at(
    base_url: &str,
    timeout: Duration,
) -> Result<BootstrapToken, BootstrapError> {
    let metadata_token = fetch_imds_metadata_token(base_url, timeout).await?;
    let role = fetch_imds_role_name(base_url, &metadata_token, timeout).await?;
    let credentials_json =
        fetch_imds_role_credentials(base_url, &metadata_token, &role, timeout).await?;
    Ok(BootstrapToken::new(credentials_json))
}

async fn fetch_imds_metadata_token(
    base_url: &str,
    timeout: Duration,
) -> Result<String, BootstrapError> {
    let endpoint = "/latest/api/token";
    let request = format!(
        "PUT {endpoint} HTTP/1.1\r\n\
         Host: 169.254.169.254\r\n\
         X-aws-ec2-metadata-token-ttl-seconds: {IMDS_TOKEN_TTL_SECONDS}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         \r\n"
    );
    let body = imds_round_trip(base_url, endpoint, &request, timeout).await?;
    let token = body.trim();
    if token.is_empty() {
        return Err(BootstrapError::ImdsEmptyToken);
    }
    Ok(token.to_owned())
}

async fn fetch_imds_role_name(
    base_url: &str,
    metadata_token: &str,
    timeout: Duration,
) -> Result<String, BootstrapError> {
    let endpoint = "/latest/meta-data/iam/security-credentials/";
    let request = format!(
        "GET {endpoint} HTTP/1.1\r\n\
         Host: 169.254.169.254\r\n\
         X-aws-ec2-metadata-token: {metadata_token}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    let body = imds_round_trip(base_url, endpoint, &request, timeout).await?;
    // The role-list endpoint returns one or more role names, newline-
    // separated. Modern EC2 instances have exactly one instance profile so
    // we take the first non-empty line.
    let role = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned();
    if role.is_empty() {
        return Err(BootstrapError::ImdsNoInstanceRole);
    }
    Ok(role)
}

async fn fetch_imds_role_credentials(
    base_url: &str,
    metadata_token: &str,
    role: &str,
    timeout: Duration,
) -> Result<String, BootstrapError> {
    let endpoint = format!("/latest/meta-data/iam/security-credentials/{role}");
    let request = format!(
        "GET {endpoint} HTTP/1.1\r\n\
         Host: 169.254.169.254\r\n\
         X-aws-ec2-metadata-token: {metadata_token}\r\n\
         Connection: close\r\n\
         \r\n"
    );
    imds_round_trip(base_url, &endpoint, &request, timeout).await
}

/// Connect to `base_url`'s host:port, write `request` verbatim, read the
/// full response (the server is expected to honor our `Connection: close`),
/// then parse out the body. Non-2xx responses surface as `ImdsHttp`.
async fn imds_round_trip(
    base_url: &str,
    endpoint: &str,
    request: &str,
    timeout: Duration,
) -> Result<String, BootstrapError> {
    let (host, port) = parse_base_url(base_url)?;
    let addr = format!("{host}:{port}");

    let connect = tokio::time::timeout(timeout, TcpStream::connect((host.as_str(), port))).await;
    let mut stream = match connect {
        Ok(Ok(s)) => s,
        Ok(Err(source)) => {
            return Err(BootstrapError::ImdsConnect { addr, source });
        }
        Err(_) => {
            return Err(BootstrapError::ImdsTimeout {
                endpoint: endpoint.to_owned(),
                timeout_ms: timeout.as_millis(),
            });
        }
    };

    let io = tokio::time::timeout(timeout, async {
        stream.write_all(request.as_bytes()).await?;
        let mut buf = Vec::with_capacity(2048);
        stream.read_to_end(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    })
    .await;
    let raw = match io {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(BootstrapError::ImdsIo(e)),
        Err(_) => {
            return Err(BootstrapError::ImdsTimeout {
                endpoint: endpoint.to_owned(),
                timeout_ms: timeout.as_millis(),
            });
        }
    };

    parse_http_response(endpoint, &raw)
}

/// Parse a minimal HTTP/1.x response. Assumes `Connection: close` semantics
/// (body terminates at EOF). IMDS responses set Content-Length and don't
/// use chunked transfer, so we don't bother decoding chunked encoding.
fn parse_http_response(endpoint: &str, raw: &[u8]) -> Result<String, BootstrapError> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| BootstrapError::ImdsMalformedResponse {
            endpoint: endpoint.to_owned(),
        })?;
    let header_bytes = &raw[..sep];
    let body_bytes = &raw[sep + 4..];

    let header_str =
        std::str::from_utf8(header_bytes).map_err(|_| BootstrapError::ImdsMalformedResponse {
            endpoint: endpoint.to_owned(),
        })?;
    let status_line =
        header_str
            .lines()
            .next()
            .ok_or_else(|| BootstrapError::ImdsMalformedResponse {
                endpoint: endpoint.to_owned(),
            })?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| BootstrapError::ImdsMalformedResponse {
            endpoint: endpoint.to_owned(),
        })?;

    let body = String::from_utf8_lossy(body_bytes).into_owned();
    if !(200..300).contains(&status) {
        return Err(BootstrapError::ImdsHttp {
            endpoint: endpoint.to_owned(),
            status,
            body,
        });
    }
    Ok(body)
}

/// Parse `http://host[:port]` into (host, port). Default port is 80. We
/// only support plain HTTP here — IMDS doesn't speak TLS.
fn parse_base_url(url: &str) -> Result<(String, u16), BootstrapError> {
    let stripped = url
        .strip_prefix("http://")
        .ok_or_else(|| BootstrapError::ImdsInvalidBaseUrl(url.to_owned()))?;
    // Tolerate (and ignore) a trailing path.
    let host_and_port = stripped.split('/').next().unwrap_or(stripped);
    if let Some((host, port_str)) = host_and_port.rsplit_once(':') {
        let port: u16 = port_str
            .parse()
            .map_err(|_| BootstrapError::ImdsInvalidBaseUrl(url.to_owned()))?;
        if host.is_empty() {
            return Err(BootstrapError::ImdsInvalidBaseUrl(url.to_owned()));
        }
        Ok((host.to_owned(), port))
    } else if host_and_port.is_empty() {
        Err(BootstrapError::ImdsInvalidBaseUrl(url.to_owned()))
    } else {
        Ok((host_and_port.to_owned(), 80))
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

    use std::sync::Arc;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::task::JoinHandle;

    /// Pre-canned response for a single IMDS endpoint.
    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        reason: &'static str,
        body: String,
    }

    impl MockResponse {
        fn ok(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                reason: "OK",
                body: body.into(),
            }
        }
        fn http(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
            Self {
                status,
                reason,
                body: body.into(),
            }
        }
    }

    /// Tiny in-process IMDS mock. Maps `request path` → response.
    /// Drives one client per connection (matches our `Connection: close`).
    struct MockImds {
        base_url: String,
        _handle: JoinHandle<()>,
        seen: Arc<AsyncMutex<Vec<String>>>,
    }

    impl MockImds {
        async fn new(responses: Vec<(&'static str, MockResponse)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://{addr}");
            let seen = Arc::new(AsyncMutex::new(Vec::new()));
            let seen_clone = seen.clone();
            let handle = tokio::spawn(async move {
                for _ in 0..responses.len() {
                    let (mut stream, _) = match listener.accept().await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    // Read until "\r\n\r\n".
                    let mut buf = [0u8; 2048];
                    let mut req = Vec::new();
                    loop {
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        req.extend_from_slice(&buf[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req_str = String::from_utf8_lossy(&req).into_owned();
                    let path = req_str
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_owned();
                    seen_clone.lock().await.push(path.clone());

                    // Match against registered paths in order; reply with
                    // the first match. Unknown paths get 404.
                    let response = responses
                        .iter()
                        .find(|(p, _)| *p == path.as_str())
                        .map(|(_, r)| r.clone())
                        .unwrap_or(MockResponse::http(404, "Not Found", ""));
                    let body = response.body;
                    let head = format!(
                        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status,
                        response.reason,
                        body.len(),
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                    let _ = stream.shutdown().await;
                }
            });
            Self {
                base_url,
                _handle: handle,
                seen,
            }
        }

        async fn seen_paths(&self) -> Vec<String> {
            self.seen.lock().await.clone()
        }
    }

    fn imds_happy_path_responses() -> Vec<(&'static str, MockResponse)> {
        let credentials = r#"{
  "Code": "Success",
  "AccessKeyId": "AKIAEXAMPLE",
  "SecretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
  "Token": "AQoEXAMPLEsessiontoken",
  "Expiration": "2026-05-24T12:00:00Z"
}"#;
        vec![
            ("/latest/api/token", MockResponse::ok("mock-imds-token")),
            (
                "/latest/meta-data/iam/security-credentials/",
                MockResponse::ok("remo-broker-role"),
            ),
            (
                "/latest/meta-data/iam/security-credentials/remo-broker-role",
                MockResponse::ok(credentials),
            ),
        ]
    }

    #[tokio::test]
    async fn imds_happy_path_returns_credentials_token() {
        let mock = MockImds::new(imds_happy_path_responses()).await;
        let token = fetch_imds_at(&mock.base_url, Duration::from_secs(2))
            .await
            .unwrap();
        // The returned BootstrapToken wraps the credentials JSON verbatim.
        assert!(token.expose().contains("AKIAEXAMPLE"));
        assert!(token.expose().contains("AQoEXAMPLEsessiontoken"));
        let paths = mock.seen_paths().await;
        assert_eq!(
            paths,
            vec![
                "/latest/api/token".to_owned(),
                "/latest/meta-data/iam/security-credentials/".to_owned(),
                "/latest/meta-data/iam/security-credentials/remo-broker-role".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn imds_500_on_token_endpoint_surfaces_imds_http() {
        let mock = MockImds::new(vec![(
            "/latest/api/token",
            MockResponse::http(500, "Internal Server Error", "boom"),
        )])
        .await;
        let err = fetch_imds_at(&mock.base_url, Duration::from_secs(2))
            .await
            .unwrap_err();
        match err {
            BootstrapError::ImdsHttp { status, body, .. } => {
                assert_eq!(status, 500);
                assert_eq!(body, "boom");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn imds_empty_role_list_returns_no_instance_role() {
        let mock = MockImds::new(vec![
            ("/latest/api/token", MockResponse::ok("tok")),
            (
                "/latest/meta-data/iam/security-credentials/",
                MockResponse::ok("\n\n"),
            ),
        ])
        .await;
        let err = fetch_imds_at(&mock.base_url, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BootstrapError::ImdsNoInstanceRole),
            "expected ImdsNoInstanceRole, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn imds_empty_token_body_is_rejected() {
        let mock = MockImds::new(vec![("/latest/api/token", MockResponse::ok(""))]).await;
        let err = fetch_imds_at(&mock.base_url, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(err, BootstrapError::ImdsEmptyToken),
            "expected ImdsEmptyToken, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn imds_connection_refused_returns_connect_error() {
        // Bind then drop to free the port (kernel might reuse it before our
        // connect, but the test is still meaningful: either we connect to
        // nothing and get ECONNREFUSED, or we connect and read 0 bytes —
        // the latter would fail later with ImdsMalformedResponse. Both
        // outcomes prove we don't silently succeed.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let base_url = format!("http://{addr}");

        let err = fetch_imds_at(&base_url, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                BootstrapError::ImdsConnect { .. }
                    | BootstrapError::ImdsIo(_)
                    | BootstrapError::ImdsMalformedResponse { .. }
            ),
            "expected connect / io / malformed-response error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn fetch_token_routes_imds_variant_to_real_path() {
        // Sanity check that Imds source no longer short-circuits to a
        // not-implemented error — it now exercises the real network path.
        // We can't run against the real IMDS in CI so we just confirm the
        // error is *not* `ImdsConnect { addr: "169.254.169.254:80", .. }`-
        // style noise; any of the structured Imds* variants is fine.
        // (Off-EC2 the connect will time out or be refused.)
        let result =
            tokio::time::timeout(Duration::from_secs(3), fetch_token(&BootstrapSource::Imds)).await;
        // Either the connect attempt completed (likely as an error since
        // we're not on EC2) or our outer timeout fires. Both are OK; we
        // just want to prove fetch_token routes through to fetch_imds_at.
        match result {
            Ok(Err(e)) => {
                let msg = e.to_string();
                assert!(
                    msg.starts_with("imds bootstrap:"),
                    "expected imds-prefixed error, got: {msg}"
                );
            }
            Ok(Ok(_)) => {
                // Wildly unlikely outside EC2; if it ever happens, the
                // returned token is a real one and the test still passes
                // the routing claim.
            }
            Err(_) => {
                // Outer timeout fired before the inner per-call timeout —
                // also acceptable, it still proves the IMDS path is
                // running and not the not-implemented stub.
            }
        }
    }

    #[test]
    fn parse_base_url_accepts_host_only() {
        let (h, p) = parse_base_url("http://example.com").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 80);
    }

    #[test]
    fn parse_base_url_accepts_host_port() {
        let (h, p) = parse_base_url("http://127.0.0.1:12345").unwrap();
        assert_eq!(h, "127.0.0.1");
        assert_eq!(p, 12345);
    }

    #[test]
    fn parse_base_url_rejects_https() {
        let err = parse_base_url("https://example.com").unwrap_err();
        assert!(matches!(err, BootstrapError::ImdsInvalidBaseUrl(_)));
    }

    #[test]
    fn parse_base_url_rejects_bad_port() {
        let err = parse_base_url("http://example.com:notaport").unwrap_err();
        assert!(matches!(err, BootstrapError::ImdsInvalidBaseUrl(_)));
    }

    #[test]
    fn parse_http_response_extracts_body_on_200() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let body = parse_http_response("/x", raw).unwrap();
        assert_eq!(body, "hello");
    }

    #[test]
    fn parse_http_response_surfaces_non_2xx_as_http_error() {
        let raw = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 3\r\n\r\nnay";
        let err = parse_http_response("/x", raw).unwrap_err();
        match err {
            BootstrapError::ImdsHttp { status, body, .. } => {
                assert_eq!(status, 401);
                assert_eq!(body, "nay");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parse_http_response_rejects_no_headers_separator() {
        let raw = b"not even close to HTTP";
        let err = parse_http_response("/x", raw).unwrap_err();
        assert!(matches!(err, BootstrapError::ImdsMalformedResponse { .. }));
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
