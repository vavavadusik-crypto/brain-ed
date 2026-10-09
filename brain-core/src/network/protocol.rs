//! The `brain-coord/1` wire protocol: strict typed request parsing,
//! an endpoint allowlist and uniform transport errors.
//!
//! This is a custom application protocol over HTTP/1.1 (or HTTPS). It is
//! intentionally NOT MCP Streamable HTTP and NOT MCP Authorization (OAuth
//! 2.1): the stdio MCP adapter stays the only MCP surface, and credentials
//! come from a local `0600` file, never from an authorization server.

use serde_json::{Map, Value, json};

/// Protocol version this module speaks on the wire.
pub const PROTOCOL_VERSION: u64 = 1;
/// The single endpoint this server answers.
pub const ENDPOINT: &str = "/v1/call";
/// Body size ceiling: one small JSON document, nothing more.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Header block ceiling.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Concurrent connections the server will serve before shedding load.
pub const MAX_CONNECTIONS: usize = 64;
/// Seconds to read one request before giving up on that connection.
pub const READ_TIMEOUT_SECS: u64 = 5;

/// Transport-level failures. Domain outcomes (busy, conflict, stale fence...)
/// stay inside `200` responses as `result.status: "error"` documents; these
/// are the codes the transport itself answers with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetError {
    /// 400: malformed JSON, unknown field, non-integer number, bad method.
    InvalidRequest(String),
    /// 401: missing, malformed, unknown, revoked or expired credential.
    Unauthorized,
    /// 403: space outside allowlist, reader writing, bad Host/Origin.
    Forbidden(String),
    /// 404: tool or path outside the allowlist.
    UnknownTool,
    /// 408/411/413/431: slow client, missing length, oversized input.
    TooLarge(String),
    /// 503: storage or listener failure. Fail-closed, never fall back.
    Unavailable(String),
    /// The file is not an auth store this code understands.
    Incompatible(String),
    /// Administration result: no such principal or credential.
    NotFound(String),
}

impl NetError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::Incompatible(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn unauthorized() -> Self {
        Self::Unauthorized
    }

    /// HTTP status for the transport layer.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::InvalidRequest(_) => 400,
            Self::Unauthorized => 401,
            Self::Forbidden(_) => 403,
            Self::UnknownTool => 404,
            Self::TooLarge(_) => 413,
            Self::Unavailable(_) => 503,
            Self::Incompatible(_) => 500,
            Self::NotFound(_) => 404,
        }
    }

    /// Uniform JSON body: nothing about principals, spaces or credentials.
    #[must_use]
    pub fn body(&self) -> Value {
        match self {
            Self::InvalidRequest(message) => {
                json!({"error": "invalid_request", "message": message})
            }
            Self::Unauthorized => json!({"error": "unauthorized"}),
            Self::Forbidden(_) => json!({"error": "forbidden"}),
            Self::UnknownTool => json!({"error": "unknown_tool"}),
            Self::TooLarge(message) => json!({"error": "too_large", "message": message}),
            Self::Unavailable(_) => json!({"error": "unavailable"}),
            Self::Incompatible(message) => json!({"error": "incompatible", "message": message}),
            Self::NotFound(_) => json!({"error": "not_found"}),
        }
    }
}

/// Marker variant used only where a size limit is known before parsing.
#[allow(dead_code)]
const INVALID_REQUEST_OF_SIZE: NetError = NetError::InvalidRequest(String::new());

impl From<std::io::Error> for NetError {
    fn from(error: std::io::Error) -> Self {
        Self::Unavailable(format!("I/O: {error}"))
    }
}

impl From<rusqlite::Error> for NetError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Unavailable(format!("storage: {error}"))
    }
}

/// A validated `POST /v1/call` body.
#[derive(Clone, Debug)]
pub struct CoordRequest {
    pub tool: String,
    pub agent: String,
    pub arguments: Map<String, Value>,
    pub request_id: Option<String>,
}

/// The tools the dispatcher will serve; everything else is `unknown_tool`.
#[must_use]
pub fn allowed_tools() -> Vec<&'static str> {
    vec![
        "work_board",
        "work_show",
        "work_claims",
        "work_add",
        "work_depend",
        "work_claim",
        "work_claim_path",
        "work_renew",
        "work_release",
        "work_set_state",
        "work_handoff",
        "work_verify",
        "work_whoami",
        "share_pull",
        "share_query",
    ]
}

/// True when `tool` mutates state; readers may call only the others.
#[must_use]
pub fn is_write_tool(tool: &str) -> bool {
    !matches!(
        tool,
        "work_board"
            | "work_show"
            | "work_claims"
            | "work_verify"
            | "work_whoami"
            | "share_pull"
            | "share_query"
    )
}

/// The allowlisted tools a credential of `role` may call (what the server
/// enforces, not a promise about any other capability).
#[must_use]
pub fn tools_for_role(role: &str) -> Vec<&'static str> {
    match role {
        "worker" => allowed_tools(),
        "reader" => allowed_tools()
            .into_iter()
            .filter(|tool| !is_write_tool(tool))
            .collect(),
        _ => Vec::new(),
    }
}

/// Rejects floats, huge numbers and strings posing as integers.
pub fn strict_i64(value: &Value, field: &str) -> Result<i64, NetError> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .ok_or_else(|| NetError::invalid(format!("{field} must be a JSON integer"))),
        _ => Err(NetError::invalid(format!("{field} must be a JSON integer"))),
    }
}

/// Positive integer with the same strictness.
pub fn strict_positive(value: &Value, field: &str) -> Result<i64, NetError> {
    let number = strict_i64(value, field)?;
    if number <= 0 {
        return Err(NetError::invalid(format!("{field} must be positive")));
    }
    Ok(number)
}

/// Bounded required string.
pub fn strict_string(value: Option<&Value>, field: &str, max: usize) -> Result<String, NetError> {
    let Some(value) = value else {
        return Err(NetError::invalid(format!("{field} is required")));
    };
    let Value::String(text) = value else {
        return Err(NetError::invalid(format!("{field} must be a string")));
    };
    if text.is_empty() || text.len() > max {
        return Err(NetError::invalid(format!(
            "{field} must be 1..={max} bytes"
        )));
    }
    if text.chars().any(|c| c.is_control()) {
        return Err(NetError::invalid(format!(
            "{field} contains control characters"
        )));
    }
    Ok(text.clone())
}

/// Optional string with the same bounds as [`strict_string`].
pub fn opt_strict_string(
    value: Option<&Value>,
    field: &str,
    max: usize,
) -> Result<Option<String>, NetError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(_) => strict_string(value, field, max).map(Some),
    }
}

/// Refuses keys the dispatcher does not know for this tool.
pub fn deny_unknown(arguments: &Map<String, Value>, known: &[&str]) -> Result<(), NetError> {
    for key in arguments.keys() {
        if !known.contains(&key.as_str()) {
            return Err(NetError::invalid(format!("unknown field: {key}")));
        }
    }
    Ok(())
}

/// Machine identity is never taken from the body: the credential is the
/// only source of `machine`. Any attempt to claim one is refused early.
pub fn deny_identity_fields(arguments: &Map<String, Value>) -> Result<(), NetError> {
    for key in [
        "machine",
        "machine_id",
        "space_owner",
        "principal",
        "token_id",
    ] {
        if arguments.contains_key(key) {
            return Err(NetError::invalid(format!(
                "{key} is not accepted; identity comes from the credential"
            )));
        }
    }
    Ok(())
}

/// Optional page size of a share read: 1..=10, default 10.
fn share_limit(args: &Map<String, Value>) -> Result<(), NetError> {
    if let Some(limit) = args.get("limit") {
        let limit = strict_i64(limit, "limit")?;
        if !(1..=10).contains(&limit) {
            return Err(NetError::invalid("limit must be 1..=10"));
        }
    }
    Ok(())
}

/// One parsed request bound to its strict schema.
impl CoordRequest {
    /// Parses a `/v1/call` body with `deny_unknown`-style strictness.
    ///
    /// # Errors
    /// `NetError::InvalidRequest` for anything malformed or impersonating.
    pub fn parse(body: &[u8]) -> Result<Self, NetError> {
        let parsed: Value = serde_json::from_slice(body)
            .map_err(|error| NetError::invalid(format!("body is not valid JSON: {error}")))?;
        let Value::Object(map) = parsed else {
            return Err(NetError::invalid("body must be a JSON object"));
        };
        for key in map.keys() {
            if !["protocol", "tool", "agent", "arguments", "request_id"].contains(&key.as_str()) {
                return Err(NetError::invalid(format!("unknown field: {key}")));
            }
        }
        if let Some(protocol) = map.get("protocol")
            && strict_i64(protocol, "protocol")? as u64 > PROTOCOL_VERSION
        {
            return Err(NetError::invalid("protocol version too new"));
        }
        let tool = strict_string(map.get("tool"), "tool", 64)?;
        if !allowed_tools().contains(&tool.as_str()) {
            return Err(NetError::UnknownTool);
        }
        // `agent` is optional: when absent the server uses the credential's
        // bound agent identity; when present it must match that identity.
        let agent = match map.get("agent") {
            Some(Value::Null) | None => String::new(),
            Some(_) => strict_string(map.get("agent"), "agent", 64)?,
        };
        if !agent.is_empty()
            && !agent
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".:_@-".contains(&b))
        {
            return Err(NetError::invalid("agent must be [A-Za-z0-9._:@-]"));
        }
        let arguments = match map.get("arguments") {
            Some(Value::Object(arguments)) => arguments.clone(),
            Some(_) => return Err(NetError::invalid("arguments must be an object")),
            None => Map::new(),
        };
        let request_id = opt_strict_string(map.get("request_id"), "request_id", 128)?;
        if let Some(id) = &request_id
            && !id.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(NetError::invalid("request_id must be printable ASCII"));
        }
        Ok(Self {
            tool,
            agent,
            arguments,
            request_id,
        })
    }

    /// The per-tool schema check, run after authentication and authorization
    /// so an attacker learns nothing about which fields exist.
    ///
    /// # Errors
    /// `NetError::InvalidRequest`.
    pub fn validate(&self) -> Result<(), NetError> {
        let args = &self.arguments;
        deny_identity_fields(args)?;
        match self.tool.as_str() {
            "work_board" => {
                deny_unknown(
                    args,
                    &["space", "project", "state", "ready", "offset", "limit"],
                )?;
                strict_string(args.get("space"), "space", 64)?;
                if let Some(state) = args.get("state") {
                    let state = strict_string(Some(state), "state", 16)?;
                    if !["queued", "active", "paused", "completed", "failed"]
                        .contains(&state.as_str())
                    {
                        return Err(NetError::invalid("unknown state"));
                    }
                }
            }
            "work_show" => {
                deny_unknown(args, &["space", "task_id"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("task_id")
                        .ok_or_else(|| NetError::invalid("task_id is required"))?,
                    "task_id",
                )?;
            }
            "work_claims" => {
                deny_unknown(args, &["space", "project", "offset", "limit"])?;
                strict_string(args.get("space"), "space", 64)?;
            }
            "work_add" => {
                deny_unknown(
                    args,
                    &[
                        "space",
                        "project",
                        "title",
                        "detail",
                        "depends_on",
                        "request_id",
                    ],
                )?;
                strict_string(args.get("space"), "space", 64)?;
                strict_string(args.get("project"), "project", 100)?;
                strict_string(args.get("title"), "title", 200)?;
                match args.get("depends_on") {
                    None | Some(Value::Null) => {}
                    Some(Value::Array(items)) => {
                        if items.len() > 32 {
                            return Err(NetError::invalid("at most 32 dependencies"));
                        }
                        for item in items {
                            strict_positive(item, "depends_on item")?;
                        }
                    }
                    Some(_) => return Err(NetError::invalid("depends_on must be an array")),
                }
            }
            "work_depend" => {
                deny_unknown(args, &["space", "task_id", "depends_on"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("task_id")
                        .ok_or_else(|| NetError::invalid("task_id is required"))?,
                    "task_id",
                )?;
                strict_positive(
                    args.get("depends_on")
                        .ok_or_else(|| NetError::invalid("depends_on is required"))?,
                    "depends_on",
                )?;
            }
            "work_claim" => {
                deny_unknown(args, &["space", "task_id", "ttl_secs", "request_id"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("task_id")
                        .ok_or_else(|| NetError::invalid("task_id is required"))?,
                    "task_id",
                )?;
                if let Some(ttl) = args.get("ttl_secs") {
                    strict_i64(ttl, "ttl_secs")?;
                }
            }
            "work_claim_path" => {
                deny_unknown(
                    args,
                    &[
                        "space",
                        "project",
                        "path",
                        "task_id",
                        "ttl_secs",
                        "request_id",
                    ],
                )?;
                strict_string(args.get("space"), "space", 64)?;
                strict_string(args.get("project"), "project", 100)?;
                strict_string(args.get("path"), "path", 1_024)?;
            }
            "work_renew" => {
                deny_unknown(args, &["space", "claim_id", "fence", "ttl_secs"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("claim_id")
                        .ok_or_else(|| NetError::invalid("claim_id is required"))?,
                    "claim_id",
                )?;
                strict_positive(
                    args.get("fence")
                        .ok_or_else(|| NetError::invalid("fence is required"))?,
                    "fence",
                )?;
                if let Some(ttl) = args.get("ttl_secs") {
                    strict_i64(ttl, "ttl_secs")?;
                }
            }
            "work_release" => {
                deny_unknown(args, &["space", "claim_id", "fence", "note"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("claim_id")
                        .ok_or_else(|| NetError::invalid("claim_id is required"))?,
                    "claim_id",
                )?;
                strict_positive(
                    args.get("fence")
                        .ok_or_else(|| NetError::invalid("fence is required"))?,
                    "fence",
                )?;
                opt_strict_string(args.get("note"), "note", 2_000)?;
            }
            "work_set_state" => {
                deny_unknown(
                    args,
                    &["space", "claim_id", "fence", "state", "note", "request_id"],
                )?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("claim_id")
                        .ok_or_else(|| NetError::invalid("claim_id is required"))?,
                    "claim_id",
                )?;
                strict_positive(
                    args.get("fence")
                        .ok_or_else(|| NetError::invalid("fence is required"))?,
                    "fence",
                )?;
                let state = strict_string(args.get("state"), "state", 16)?;
                if !["queued", "paused", "completed", "failed"].contains(&state.as_str()) {
                    return Err(NetError::invalid("unknown state"));
                }
                opt_strict_string(args.get("note"), "note", 2_000)?;
            }
            "work_handoff" => {
                deny_unknown(
                    args,
                    &[
                        "space",
                        "claim_id",
                        "fence",
                        "to_agent",
                        "to_machine",
                        "ttl_secs",
                        "note",
                        "request_id",
                    ],
                )?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("claim_id")
                        .ok_or_else(|| NetError::invalid("claim_id is required"))?,
                    "claim_id",
                )?;
                strict_positive(
                    args.get("fence")
                        .ok_or_else(|| NetError::invalid("fence is required"))?,
                    "fence",
                )?;
                strict_string(args.get("to_agent"), "to_agent", 64)?;
                let to_machine = strict_string(args.get("to_machine"), "to_machine", 64)?;
                if !to_machine
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                {
                    return Err(NetError::invalid("to_machine must be [a-z0-9-]"));
                }
                if let Some(ttl) = args.get("ttl_secs") {
                    strict_i64(ttl, "ttl_secs")?;
                }
                opt_strict_string(args.get("note"), "note", 2_000)?;
            }
            "work_verify" => {
                deny_unknown(args, &["space", "claim_id", "fence"])?;
                strict_string(args.get("space"), "space", 64)?;
                strict_positive(
                    args.get("claim_id")
                        .ok_or_else(|| NetError::invalid("claim_id is required"))?,
                    "claim_id",
                )?;
                strict_positive(
                    args.get("fence")
                        .ok_or_else(|| NetError::invalid("fence is required"))?,
                    "fence",
                )?;
            }
            "work_whoami" => {
                deny_unknown(args, &[])?;
            }
            // Shared memory reads: the space must be in the credential's
            // namespace (checked by authorize), granted by host-local admin
            // and enabled by the host (checked by the share store). Nothing
            // here can name an origin, agent, machine or grant.
            "share_pull" => {
                deny_unknown(args, &["space", "after_seq", "limit"])?;
                strict_string(args.get("space"), "space", 64)?;
                let after = strict_i64(
                    args.get("after_seq").unwrap_or(&Value::from(0)),
                    "after_seq",
                )?;
                if !(0..=(1_i64 << 53)).contains(&after) {
                    return Err(NetError::invalid("after_seq must be 0..=2^53"));
                }
                share_limit(args)?;
            }
            "share_query" => {
                deny_unknown(args, &["space", "limit"])?;
                strict_string(args.get("space"), "space", 64)?;
                share_limit(args)?;
            }
            _ => return Err(NetError::UnknownTool),
        }
        Ok(())
    }

    /// The space this request wants to act in.
    #[must_use]
    pub fn space(&self) -> Option<&str> {
        self.arguments.get("space").and_then(Value::as_str)
    }

    /// True when the tool mutates state (used for the reader-role gate).
    #[must_use]
    pub fn is_write(&self) -> bool {
        is_write_tool(&self.tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_request() {
        let body = br#"{"tool":"work_claim","agent":"codex","arguments":{"space":"team-a","task_id":7,"ttl_secs":1800},"request_id":"ad-1"}"#;
        let request = CoordRequest::parse(body).expect("valid");
        assert_eq!(request.tool, "work_claim");
        assert_eq!(request.agent, "codex");
        assert!(request.is_write());
        assert_eq!(request.space(), Some("team-a"));
        request.validate().expect("schema");
    }

    #[test]
    fn rejects_strings_floats_and_overflow_as_integers() {
        for bad in [
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":"7"}}"#.as_slice(),
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":7.0}}"#.as_slice(),
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":1e2}}"#.as_slice(),
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":-1}}"#.as_slice(),
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":9223372036854775808}}"#.as_slice(),
        ] {
            let request = CoordRequest::parse(bad).expect("parses");
            assert!(request.validate().is_err(), "must refuse {bad:?}");
        }
    }

    #[test]
    fn rejects_unknown_fields_and_tools_and_identity_grabs() {
        let unknown_tool = CoordRequest::parse(br#"{"tool":"sql","agent":"a","arguments":{}}"#);
        assert_eq!(unknown_tool.unwrap_err(), NetError::UnknownTool);
        let memory = CoordRequest::parse(br#"{"tool":"memory_search","agent":"a","arguments":{}}"#);
        assert_eq!(memory.unwrap_err(), NetError::UnknownTool);
        let archive = CoordRequest::parse(br#"{"tool":"archive_read","agent":"a","arguments":{}}"#);
        assert_eq!(archive.unwrap_err(), NetError::UnknownTool);
        let top =
            CoordRequest::parse(br#"{"tool":"work_claim","agent":"a","arguments":{},"extra":1}"#);
        assert!(matches!(top.unwrap_err(), NetError::InvalidRequest(_)));
        let field = CoordRequest::parse(
            br#"{"tool":"work_claim","agent":"a","arguments":{"space":"s","task_id":1,"machine":"m-evil"}}"#,
        )
        .expect("parse");
        assert!(matches!(
            field.validate().unwrap_err(),
            NetError::InvalidRequest(_)
        ));
    }

    #[test]
    fn share_reads_refuse_caller_claimed_authority_and_bad_bounds() {
        let ok = CoordRequest::parse(
            br#"{"tool":"share_pull","arguments":{"space":"team","after_seq":0,"limit":10}}"#,
        )
        .expect("parse");
        ok.validate().expect("valid pull");
        assert!(!ok.is_write(), "share reads are reads");
        for body in [
            br#"{"tool":"share_pull","arguments":{"space":"team","grant":"memory:read:team"}}"#
                .as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","machine":"m-host"}}"#.as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","origin":"install-x"}}"#
                .as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","trusted_peer":"host"}}"#
                .as_slice(),
            br#"{"tool":"share_query","arguments":{"space":"team","authority":true}}"#.as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","limit":11}}"#.as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","after_seq":-1}}"#.as_slice(),
            br#"{"tool":"share_pull","arguments":{"space":"team","after_seq":1.5}}"#.as_slice(),
        ] {
            let request = CoordRequest::parse(body).expect("parses");
            assert!(
                request.validate().is_err(),
                "must refuse {}",
                String::from_utf8_lossy(body)
            );
        }
        for tool in [
            "share_publish",
            "share_apply",
            "share_grant",
            "memory_write",
        ] {
            let body = format!(r#"{{"tool":"{tool}","arguments":{{"space":"team"}}}}"#);
            assert_eq!(
                CoordRequest::parse(body.as_bytes()).unwrap_err(),
                NetError::UnknownTool
            );
        }
    }

    #[test]
    fn protocol_version_limit_is_enforced() {
        let future = CoordRequest::parse(
            br#"{"tool":"work_whoami","agent":"a","protocol":2,"arguments":{}}"#,
        );
        assert!(matches!(future.unwrap_err(), NetError::InvalidRequest(_)));
        let now = CoordRequest::parse(
            br#"{"tool":"work_whoami","agent":"a","protocol":1,"arguments":{}}"#,
        );
        assert!(now.is_ok());
    }

    #[test]
    fn malformed_json_is_invalid_request() {
        let error = CoordRequest::parse(b"{not json").unwrap_err();
        assert_eq!(error.status(), 400);
    }
}
