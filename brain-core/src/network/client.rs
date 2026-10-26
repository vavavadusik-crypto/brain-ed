//! Client side of `brain-coord/1` for future CLI/MCP adapters.
//!
//! Plain-TCP implementation over `std::net::TcpStream`; the TLS client
//! (pinned CA, absolute deadline) lives in [`crate::network::tls`]. The client fails closed: connection errors are returned,
//! never retried against a different endpoint, and there is no local
//! fallback coordinator. The credential is read from a `0600` file each
//! call and never appears in argv, URLs, logs or error messages.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::network::protocol::{
    CoordRequest, ENDPOINT, MAX_BODY_BYTES, NetError, PROTOCOL_VERSION,
};
use crate::network::tls::KeySource;

/// Seconds to connect and to read one answer.
pub const CONNECT_TIMEOUT_SECS: u64 = 3;
pub const REQUEST_TIMEOUT_SECS: u64 = 10;

/// Configuration the adapter will expose as CLI flags: no env vars.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// `http://127.0.0.1:PORT` of the coordinator (numeric loopback only).
    pub endpoint: String,
    /// Path to the `0600` credential file containing `brc1.<id>.<secret>`.
    pub credential_file: PathBuf,
    /// Optional space to prefix calls with when the tool needs one.
    pub default_space: Option<String>,
}

/// A client bound to one coordinator and one credential.
pub struct CoordClient {
    config: ClientConfig,
    token: String,
}

/// Outcome of one call: the coordinator's answer or a transport failure.
pub enum CallResult {
    /// HTTP 200 with the full protocol envelope.
    Ok(Value),
    /// Transport-level refusal (401/403/404/413/503...).
    Refused(NetError),
    /// Connection or I/O failure. Retrying the SAME request_id is safe.
    Unreachable(String),
}

impl CoordClient {
    /// Loads the credential file, checking it is owner-only.
    ///
    /// # Errors
    /// `NetError::InvalidRequest` when the file is missing, world-readable
    /// or does not contain a parsable token.
    pub fn new(config: ClientConfig) -> Result<Self, NetError> {
        let token = read_credential(&config.credential_file)?;
        Ok(Self { config, token })
    }

    /// Endpoint this client talks to.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.config.endpoint
    }

    /// Calls one allowlisted tool. `arguments` must already match the
    /// schema; unknown keys are refused by the server anyway.
    ///
    /// # Errors
    /// [`CallResult::Refused`] or [`CallResult::Unreachable`] — this method
    /// itself returns nothing; inspect the enum.
    pub fn call(
        &self,
        tool: &str,
        agent: &str,
        arguments: Map<String, Value>,
        request_id: Option<String>,
    ) -> CallResult {
        let request = CoordRequest {
            tool: tool.to_owned(),
            agent: agent.to_owned(),
            arguments,
            request_id,
        };
        self.call_typed(&request)
    }

    fn call_typed(&self, request: &CoordRequest) -> CallResult {
        let mut body = json!({
            "protocol": PROTOCOL_VERSION,
            "tool": request.tool,
            "agent": request.agent,
            "arguments": request.arguments,
        });
        if let Some(id) = &request.request_id {
            body["request_id"] = json!(id);
        }
        let payload = match serde_json::to_vec(&body) {
            Ok(payload) => payload,
            Err(error) => return CallResult::Unreachable(format!("encode: {error}")),
        };
        if payload.len() > MAX_BODY_BYTES {
            return CallResult::Refused(NetError::TooLarge("request too large".into()));
        }
        // Numeric loopback only: a hostname is refused, never resolved.
        let target = self
            .config
            .endpoint
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let address = match crate::network::tls::parse_dial_target(target) {
            Ok(address) => address,
            Err(error) => {
                return CallResult::Refused(NetError::invalid(format!(
                    "endpoint: {}",
                    error.code()
                )));
            }
        };
        let authority = address.ip().to_string();
        let stream =
            TcpStream::connect_timeout(&address, Duration::from_secs(CONNECT_TIMEOUT_SECS));
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => return CallResult::Unreachable(format!("connect: {error}")),
        };
        if stream
            .set_read_timeout(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
            .is_err()
            || stream
                .set_write_timeout(Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
                .is_err()
        {
            return CallResult::Unreachable("cannot set timeouts".into());
        }
        // The token goes into the Authorization header only.
        let head = format!(
            "POST {ENDPOINT} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.token,
            payload.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&payload).is_err() {
            return CallResult::Unreachable("write failed".into());
        }
        let mut response = Vec::new();
        if stream.read_to_end(&mut response).is_err() {
            return CallResult::Unreachable("read failed (retry the same request_id)".into());
        }
        parse_response(&response)
    }
}

pub(crate) fn parse_response(response: &[u8]) -> CallResult {
    let text = String::from_utf8_lossy(response);
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .and_then(|body| serde_json::from_str::<Value>(body).ok());
    match (status, body) {
        (200, Some(body)) => CallResult::Ok(body),
        (200, None) => CallResult::Unreachable("server sent an unparseable 200".into()),
        (status, body) => CallResult::Refused(match status {
            400 => NetError::invalid(
                body.as_ref()
                    .and_then(|body| body.get("message").and_then(Value::as_str))
                    .unwrap_or("invalid request"),
            ),
            401 => NetError::unauthorized(),
            403 => NetError::forbidden(
                body.as_ref()
                    .and_then(|body| body.get("message").and_then(Value::as_str))
                    .unwrap_or("forbidden"),
            ),
            404 => NetError::UnknownTool,
            413 | 431 => NetError::TooLarge("request too large".into()),
            _ => NetError::unavailable(format!("status {status}")),
        }),
    }
}

/// Loads a credential through ONE opened file descriptor: the permission
/// and symlink checks run on the descriptor itself (via its metadata), then
/// the bytes are read from the same descriptor — nothing re-resolves the
/// path, so there is no stat→read TOCTOU window.
///
/// # Errors
/// `NetError::InvalidRequest` when the file is missing, a symlink, not a
/// regular owner-only file, oversized, or does not hold a parsable token.
pub fn read_credential(path: &Path) -> Result<String, NetError> {
    read_credential_from(&KeySource::File(path.to_path_buf()))
}

/// As [`read_credential`], from a file or an inherited descriptor
/// (`KeySource::Fd`, `>= 3`). A descriptor gets the same checks on what it
/// actually refers to: a pipe, socket, FIFO, foreign-uid or group/world
/// accessible file is refused, and opening never blocks.
///
/// # Errors
/// As [`read_credential`].
pub fn read_credential_from(source: &KeySource) -> Result<String, NetError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    const MAX: u64 = 4 * 1024;
    // Real libc constants, never magic numbers. O_NONBLOCK makes opening a
    // FIFO return instead of hanging; O_NOFOLLOW refuses symlinks at open.
    let file = match source {
        KeySource::File(path) => std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| NetError::invalid("credential file not found or is a symlink"))?,
        KeySource::Fd(fd) => crate::network::tls::open_inherited_fd(*fd).map_err(|_| {
            NetError::invalid("credential descriptor is not an open descriptor >= 3")
        })?,
    };
    let metadata = file
        .metadata()
        .map_err(|_| NetError::invalid("credential file metadata unreadable"))?;
    if !metadata.is_file() {
        return Err(NetError::invalid("credential path is not a regular file"));
    }
    if metadata.len() > MAX {
        return Err(NetError::invalid("credential file is too large"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(NetError::invalid("credential file must be 0600 owner-only"));
    }
    if crate::network::tls::current_uid() != Some(metadata.uid()) {
        return Err(NetError::invalid(
            "credential file is not owned by this uid",
        ));
    }
    let mut raw = String::new();
    file.take(MAX + 1)
        .read_to_string(&mut raw)
        .map_err(|_| NetError::invalid("credential file unreadable"))?;
    let parsed = crate::network::auth::BearerToken::parse(raw.trim())
        .map_err(|_| NetError::invalid("credential file does not hold a brc1 token"))?;
    Ok(parsed.as_str().to_owned())
}

/// Writes a credential file safely: refuses to replace, `0600`. Missing
/// parent directories are created `0700`; an EXISTING directory's mode is
/// never changed (it may be the user's home).
///
/// # Errors
/// `NetError::InvalidRequest` on any failure.
pub fn write_credential_file(path: &Path, token: &str) -> Result<(), NetError> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && !parent.is_dir()
    {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| NetError::invalid("cannot create 0700 credential directory"))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| NetError::invalid("credential file exists or is not creatable"))?;
    file.write_all(token.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|_| NetError::invalid("cannot write credential"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn temp(_tag: &str) -> PathBuf {
        let dir = crate::test_support::temp_path("brain-client");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn config(path: PathBuf) -> ClientConfig {
        ClientConfig {
            endpoint: "http://127.0.0.1:1".into(),
            credential_file: path,
            default_space: None,
        }
    }

    fn token_file(dir: &Path, name: &str, mode: u32, content: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write");
        std::fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("perm");
        path
    }

    #[test]
    fn refuses_a_missing_credential_file() {
        let dir = temp("missing");
        assert!(CoordClient::new(config(dir.join("nope"))).is_err());
    }

    #[test]
    fn refuses_a_world_readable_credential_file() {
        let dir = temp("perm");
        let path = token_file(&dir, "cred", 0o644, "brc1.t1.abc");
        assert!(
            CoordClient::new(config(path)).is_err(),
            "0644 must be refused"
        );
    }

    #[test]
    fn refuses_a_symlinked_credential_file() {
        let dir = temp("link");
        let real = token_file(&dir, "real", 0o600, "brc1.t1.abc");
        let link = dir.join("link");
        symlink(&real, &link).expect("symlink");
        assert!(
            CoordClient::new(config(link)).is_err(),
            "a symlink must be refused"
        );
    }

    #[test]
    fn refuses_content_that_is_not_a_brc1_token() {
        let dir = temp("content");
        let path = token_file(&dir, "cred", 0o600, "not a token at all");
        assert!(CoordClient::new(config(path)).is_err());
    }

    #[test]
    fn an_unreachable_endpoint_is_reported_not_swallowed() {
        let dir = temp("unreach");
        let path = token_file(&dir, "cred", 0o600, "brc1.t1.abc");
        let client = CoordClient::new(config(path)).expect("client");
        let outcome = client.call("work_whoami", "codex", Map::new(), None);
        assert!(
            matches!(outcome, CallResult::Unreachable(_)),
            "no local fallback, ever"
        );
    }

    #[test]
    fn a_hostname_endpoint_is_refused_not_resolved() {
        let dir = temp("hostname");
        let path = token_file(&dir, "cred", 0o600, "brc1.t1.abc");
        for endpoint in [
            "http://localhost:1",
            "http://example.invalid:80",
            "http://10.0.0.1:1",
        ] {
            let client = CoordClient::new(ClientConfig {
                endpoint: endpoint.into(),
                credential_file: path.clone(),
                default_space: None,
            })
            .expect("client");
            let outcome = client.call("work_whoami", "codex", Map::new(), None);
            assert!(
                matches!(outcome, CallResult::Refused(NetError::InvalidRequest(_))),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn write_credential_file_is_create_new_and_0600() {
        let dir = temp("writecred");
        let path = dir.join("cred");
        write_credential_file(&path, "brc1.t1.abc").expect("write");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential must be owner-only");
        assert!(
            write_credential_file(&path, "brc1.t2.def").is_err(),
            "no replacing"
        );
    }
}
