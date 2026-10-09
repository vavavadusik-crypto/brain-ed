//! `brain-coord/1` server: a real loopback TCP server over the existing
//! `Coordinator` and `AuthStore`.
//!
//! Defaults are closed: it binds `127.0.0.1`, refuses `Origin`, requires a
//! loopback `Host`, and answers only `POST /v1/call`. Remote binding is a
//! separate configuration that requires TLS material; this module refuses
//! to start without it. There is no unauthenticated health or metadata route.
//!
//! The order of checks per request is fixed: transport bounds →
//! authentication → authorization (space allowlist, role) → schema →
//! coordinator. Identity comes from the credential, never from the body,
//! so a `machine` in `arguments` is an error and a spoofed `agent` label
//! only changes bookkeeping, not rights.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::coordination::{Actor, CoordError, Coordinator, DEFAULT_TTL_SECS, ErrorKind};
use crate::network::auth::{AuthStore, Authenticated, BearerToken};
use crate::network::protocol::{
    CoordRequest, ENDPOINT, MAX_BODY_BYTES, MAX_CONNECTIONS, MAX_HEADER_BYTES, NetError,
    PROTOCOL_VERSION, READ_TIMEOUT_SECS,
};
use crate::sharing::{
    MAX_WIRE_PAGE, Principal as SharePrincipal, ShareError, ShareEvent, ShareStore,
};

/// How the listener is bound.
#[derive(Clone, Debug)]
pub enum BindMode {
    /// `127.0.0.1:port`, plaintext HTTP. The default and the only mode without TLS.
    Loopback,
}

/// Where the server keeps `coord.sqlite` and `auth.sqlite`.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub state_dir: PathBuf,
    pub port: u16,
}

impl ServerConfig {
    pub fn new(state_dir: impl Into<PathBuf>, port: u16) -> Self {
        Self {
            state_dir: state_dir.into(),
            port,
        }
    }
}

/// A running server. Drop stops accepting; the listener thread ends.
pub struct CoordServer {
    listener: TcpListener,
    port: u16,
    inner: Arc<Inner>,
}

/// Both stores share one lock: rusqlite connections are Send but not Sync,
/// and every write path is one `BEGIN IMMEDIATE` transaction anyway.
pub(crate) struct Inner {
    state: Mutex<State>,
    live: AtomicUsize,
    /// Set once by an owned stop: every request that reaches the state
    /// lock afterwards is refused before dispatch.
    stopped: AtomicBool,
}

impl Inner {
    /// Opens both stores under `state_dir` without binding any socket, for
    /// transports (TLS) that own their own listener. Fails closed.
    pub(crate) fn open(state_dir: &std::path::Path) -> Result<Self, NetError> {
        let auth = AuthStore::open(state_dir)?;
        let coordinator = Coordinator::open(&state_dir.join("coord.sqlite"))
            .map_err(|error| NetError::unavailable(format!("coordinator: {error}")))?;
        let share = ShareStore::open(state_dir.join(SHARE_STORE_FILE))
            .map_err(|error| NetError::unavailable(format!("share store: {}", error.kind())))?;
        Ok(Self {
            state: Mutex::new(State {
                auth,
                coordinator,
                share,
            }),
            live: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
        })
    }

    /// Serves exactly one request on an already-established byte stream
    /// (plaintext socket or decrypted TLS stream) through the SAME bounded
    /// reader, check order and router; the stream's own deadlines bound it.
    pub(crate) fn serve_stream<S: Read + Write>(&self, mut stream: S) {
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err(error) => {
                respond(&mut stream, error.status(), &error.body());
                return;
            }
        };
        let outcome = route(self, &request);
        respond(&mut stream, outcome.status, &outcome.body);
    }

    /// Refuses all later dispatch. Idempotent.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub(crate) fn auth_admin<T>(
        &self,
        admin: impl FnOnce(&AuthStore) -> Result<T, NetError>,
    ) -> Result<T, NetError> {
        let state = self
            .state
            .lock()
            .map_err(|_| NetError::unavailable("server state lock poisoned"))?;
        admin(&state.auth)
    }
}

struct State {
    auth: AuthStore,
    coordinator: Coordinator,
    share: ShareStore,
}

/// The installation's shared-memory store inside the coordinator state dir.
pub const SHARE_STORE_FILE: &str = "share.sqlite";

/// Outcome of serving one request, for tests and the future CLI/MCP adapters.
pub struct CallOutcome {
    pub status: u16,
    pub body: Value,
}

impl CoordServer {
    /// Starts on `127.0.0.1`. Fails closed if either store cannot open.
    ///
    /// # Errors
    /// `NetError::Unavailable` when binding or opening a store fails.
    pub fn start(config: &ServerConfig) -> Result<Self, NetError> {
        let inner = Inner::open(&config.state_dir)?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let listener = TcpListener::bind(address)
            .map_err(|error| NetError::unavailable(format!("bind {address}: {error}")))?;
        let port = listener
            .local_addr()
            .map(|addr| addr.port())
            .unwrap_or(config.port);
        Ok(Self {
            listener,
            port,
            inner: Arc::new(inner),
        })
    }

    /// Port actually bound (useful when tests asked for port 0).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Local address for clients.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Shared inner state, for tests.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }

    /// Runs one local administration function against the auth store under
    /// the state lock: principal/credential management stays in-process.
    ///
    /// # Errors
    /// Whatever `admin` returns, or `NetError::Unavailable` if the lock is poisoned.
    pub fn auth_admin<T>(
        &self,
        admin: impl FnOnce(&AuthStore) -> Result<T, NetError>,
    ) -> Result<T, NetError> {
        self.inner.auth_admin(admin)
    }

    /// Serves on a background thread until the returned handle is dropped,
    /// which stops accepting and joins the thread. Nothing is leaked.
    pub fn serve(mut self) -> ServerHandle {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let port = self.port;
        let listener = self.listener.try_clone().expect("listener clone");
        let inner = Arc::clone(&self.inner);
        let handle = thread::Builder::new()
            .name("brain-coord-accept".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    if inner.live.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                        drop(stream);
                        continue;
                    }
                    inner.live.fetch_add(1, Ordering::SeqCst);
                    let spawned = thread::Builder::new()
                        .name("brain-coord-conn".to_owned())
                        .spawn({
                            let inner = Arc::clone(&inner);
                            move || {
                                handle_connection(stream, &inner);
                                inner.live.fetch_sub(1, Ordering::SeqCst);
                            }
                        });
                    if spawned.is_err() {
                        inner.live.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })
            .expect("spawn accept thread");
        self.port = port;
        ServerHandle {
            shutdown,
            join: Some(handle),
            port,
        }
    }
}

/// Owned background server: dropping it stops the accept loop and joins it.
pub struct ServerHandle {
    shutdown: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    port: u16,
}

impl ServerHandle {
    /// Port the background listener is bound to.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the blocking accept with one throwaway connection, then join.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn handle_connection(stream: TcpStream, inner: &Inner) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));
    inner.serve_stream(stream);
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads one line, never buffering more than `limit` bytes of it.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    limit: usize,
) -> std::io::Result<usize> {
    (&mut *reader).take(limit as u64 + 1).read_line(line)
}

fn read_request<R: Read>(stream: R) -> Result<HttpRequest, NetError> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    read_bounded_line(&mut reader, &mut request_line, MAX_HEADER_BYTES)
        .map_err(|_| NetError::TooLarge("unreadable request line".into()))?;
    if request_line.len() > MAX_HEADER_BYTES {
        return Err(NetError::TooLarge("request line exceeds 8 KiB".into()));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_ascii_uppercase();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), query.to_owned()),
        None => (target, String::new()),
    };
    // A token in the query string would leak into logs; refuse early.
    if query.to_ascii_lowercase().contains("token") {
        return Err(NetError::Unauthorized);
    }
    let mut headers = Vec::new();
    let mut header_bytes = 0_usize;
    loop {
        let mut line = String::new();
        read_bounded_line(&mut reader, &mut line, MAX_HEADER_BYTES)
            .map_err(|_| NetError::TooLarge("unreadable headers".into()))?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(NetError::TooLarge("headers exceed 8 KiB".into()));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    if headers.iter().any(|(name, _)| name == "transfer-encoding") {
        return Err(NetError::invalid("chunked bodies are not supported"));
    }
    let declared: usize = match headers.iter().find(|(name, _)| name == "content-length") {
        Some((_, value)) => value
            .parse()
            .map_err(|_| NetError::invalid("bad content-length"))?,
        None => 0,
    };
    if declared > MAX_BODY_BYTES {
        return Err(NetError::TooLarge(format!(
            "body exceeds {MAX_BODY_BYTES} bytes"
        )));
    }
    let mut body = vec![0_u8; declared];
    if declared > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|_| NetError::invalid("body shorter than declared"))?;
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn respond<W: Write>(stream: &mut W, status: u16, body: &Value) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let payload =
        serde_json::to_vec(body).unwrap_or_else(|_| b"{\"error\":\"unavailable\"}".to_vec());
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nWWW-Authenticate: Bearer realm=\"brain-coord\"\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

/// Fixed check order: route → Host → Origin → method → auth → authz → schema → coordinator.
fn route(inner: &Inner, request: &HttpRequest) -> CallOutcome {
    if request.path != ENDPOINT {
        return CallOutcome {
            status: 404,
            body: NetError::UnknownTool.body(),
        };
    }
    let host = request.header("host").unwrap_or_default();
    let expected_loopback = format!("127.0.0.1:{}", host.rsplit(':').next().unwrap_or(""));
    let host_ok = host == "127.0.0.1"
        || host == "localhost"
        || host == expected_loopback
        || host == format!("localhost:{}", host.rsplit(':').next().unwrap_or(""));
    if !host_ok {
        return CallOutcome {
            status: 403,
            body: NetError::forbidden("bad Host").body(),
        };
    }
    if request.header("origin").is_some() {
        return CallOutcome {
            status: 403,
            body: NetError::forbidden("Origin is not accepted").body(),
        };
    }
    if request.method != "POST" {
        return CallOutcome {
            status: 405,
            body: NetError::invalid("POST only").body(),
        };
    }
    let authorization = request.header("authorization").unwrap_or_default();
    let raw_token = authorization.strip_prefix("Bearer ").unwrap_or("");
    let token = match BearerToken::parse(raw_token) {
        Ok(token) => token,
        Err(_) => {
            return CallOutcome {
                status: 401,
                body: NetError::unauthorized().body(),
            };
        }
    };
    let parsed = match CoordRequest::parse(&request.body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return CallOutcome {
                status: error.status(),
                body: error.body(),
            };
        }
    };
    // One state lock covers authentication, authorization, schema and the
    // coordinator call: identity is bound before any idempotency lookup.
    let mut state = match inner.state.lock() {
        Ok(state) => state,
        Err(_) => {
            return CallOutcome {
                status: 503,
                body: NetError::unavailable("server state lock poisoned").body(),
            };
        }
    };
    if inner.stopped.load(Ordering::SeqCst) {
        return CallOutcome {
            status: 503,
            body: NetError::unavailable("server is stopping").body(),
        };
    }
    let now = unix_now();
    let authenticated = match state.auth.authenticate(&token, now, "127.0.0.1") {
        Ok(authenticated) => authenticated,
        Err(_) => {
            return CallOutcome {
                status: 401,
                body: NetError::unauthorized().body(),
            };
        }
    };
    if let Err(error) = authorize(&authenticated, &parsed) {
        return CallOutcome {
            status: error.status(),
            body: error.body(),
        };
    }
    if let Err(error) = parsed.validate() {
        return CallOutcome {
            status: error.status(),
            body: error.body(),
        };
    }
    dispatch(&mut state, &authenticated, &parsed, now)
}

fn authorize(authenticated: &Authenticated, request: &CoordRequest) -> Result<(), NetError> {
    let principal = &authenticated.principal;
    // Identity binding: a request agent, when present, must equal the
    // credential's bound agent. An omitted agent is filled in from the
    // credential; a mismatched one is refused, whatever the tool.
    if !request.agent.is_empty() && !authenticated.agent_matches(&request.agent) {
        return Err(NetError::forbidden("agent does not match the credential"));
    }
    if let Some(space) = request.space() {
        if !principal.allows_space(space) {
            return Err(NetError::forbidden("space not granted"));
        }
    } else if request.tool != "work_whoami" {
        return Err(NetError::invalid("space is required"));
    }
    if request.is_write() && !principal.may_write() {
        return Err(NetError::forbidden("readers may not write"));
    }
    Ok(())
}

fn dispatch(
    state: &mut State,
    authenticated: &Authenticated,
    request: &CoordRequest,
    now: i64,
) -> CallOutcome {
    // The actor agent is the credential's identity, never the body's label.
    let bound_agent = authenticated.agent_id();
    if request.tool == "work_whoami" {
        let body = json!({
            "protocol": PROTOCOL_VERSION,
            "server_time": now,
            "principal": {
                "agent": bound_agent,
                "machine": authenticated.principal.machine_id,
                "label": authenticated.principal.label,
                "role": authenticated.principal.role,
                "spaces": authenticated.principal.spaces,
            },
        });
        return CallOutcome { status: 200, body };
    }
    if request.tool.starts_with("share_") {
        return dispatch_share(&state.share, authenticated, request, now);
    }
    let actor = Actor::new(
        request.space().unwrap_or_default(),
        bound_agent,
        &authenticated.principal.machine_id,
    );
    let actor = match actor {
        Ok(actor) => actor,
        Err(error) => {
            return CallOutcome {
                status: 400,
                body: json!({"error": "invalid_request", "message": error.to_string()}),
            };
        }
    };
    let result = call_coordinator(&mut state.coordinator, &actor, request);
    match result {
        Ok(value) => CallOutcome {
            status: 200,
            body: json!({
                "protocol": PROTOCOL_VERSION,
                "server_time": now,
                "principal": {"machine": authenticated.principal.machine_id, "label": authenticated.principal.label},
                "result": value,
            }),
        },
        Err(error) => CallOutcome {
            status: 200,
            body: json!({
                "protocol": PROTOCOL_VERSION,
                "server_time": now,
                "principal": {"machine": authenticated.principal.machine_id},
                "result": error.to_json(),
            }),
        },
    }
}

/// Shared-memory reads. The reader principal is built from the
/// authenticated credential only; the host's enabled spaces and the
/// credential's explicit grants are intersected inside the store before
/// any row is read. Denials are one uniform 403 `share_denied`, so a caller
/// cannot probe which spaces the host enabled.
fn dispatch_share(
    share: &ShareStore,
    authenticated: &Authenticated,
    request: &CoordRequest,
    now: i64,
) -> CallOutcome {
    let reader = SharePrincipal::from_authenticated(authenticated);
    let args = &request.arguments;
    let space = request.space().unwrap_or_default();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(MAX_WIRE_PAGE, |l| l as u32);
    let result = match request.tool.as_str() {
        "share_pull" => {
            let after = args.get("after_seq").and_then(Value::as_u64).unwrap_or(0);
            share.page_own(&reader, space, after, limit).map(|page| {
                json!({
                    "origin": page.origin,
                    "space": space,
                    "after_seq": after,
                    "next_seq": page.next_seq,
                    "has_more": page.has_more,
                    "events": page.events.iter().map(ShareEvent::to_wire).collect::<Vec<_>>(),
                })
            })
        }
        "share_query" => share.own_live_facts(&reader, space, limit).map(|facts| {
            json!({
                "origin": share.install_id(),
                "space": space,
                "facts": facts.iter().map(|fact| json!({
                    "fact_id": fact.fact_id,
                    "kind": fact.kind.as_str(),
                    "payload": fact.payload,
                    "claim": fact.claim,
                    "origin_seq": fact.origin_seq,
                    "last_seq": fact.last_seq,
                    "claimed_agent": fact.claimed_agent,
                })).collect::<Vec<_>>(),
                "provenance": "claimed by this host's origin; not verified beyond the TLS session",
            })
        }),
        _ => {
            return CallOutcome {
                status: 404,
                body: NetError::UnknownTool.body(),
            };
        }
    };
    match result {
        Ok(value) => CallOutcome {
            status: 200,
            body: json!({
                "protocol": PROTOCOL_VERSION,
                "server_time": now,
                "principal": {"machine": authenticated.principal.machine_id, "agent": authenticated.agent_id()},
                "result": {"status": "ok", "share": value},
            }),
        },
        Err(ShareError::CapabilityDenied { .. } | ShareError::SpaceNotEnabled(_)) => CallOutcome {
            status: 403,
            body: json!({"error": "forbidden", "code": "share_denied"}),
        },
        Err(error @ (ShareError::PageLimit { .. } | ShareError::InvalidData(_))) => CallOutcome {
            status: 400,
            body: json!({"error": "invalid_request", "code": error.kind()}),
        },
        Err(error) => CallOutcome {
            status: 503,
            body: json!({"error": "unavailable", "code": error.kind()}),
        },
    }
}

/// Maps a validated request onto the coordinator's methods, keeping
/// domain errors (busy, conflict, stale fence) inside `200 result.status`.
fn call_coordinator(
    coordinator: &mut Coordinator,
    actor: &Actor,
    request: &CoordRequest,
) -> Result<Value, CoordError> {
    let args = &request.arguments;
    let string = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or_default();
    let integer = |name: &str| args.get(name).and_then(Value::as_i64).unwrap_or(0);
    let optional = |name: &str| args.get(name).and_then(Value::as_i64);
    let request_id = request.request_id.as_deref();
    match request.tool.as_str() {
        "work_board" => coordinator.board(
            string("space"),
            args.get("project").and_then(Value::as_str),
            args.get("state").and_then(Value::as_str),
            args.get("ready").and_then(Value::as_bool).unwrap_or(false),
            args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
            args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize,
        ),
        "work_show" => coordinator.show(string("space"), integer("task_id")),
        "work_claims" => coordinator.claims(
            string("space"),
            args.get("project").and_then(Value::as_str),
            args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
            args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize,
        ),
        "work_add" => {
            let depends: Vec<i64> = args
                .get("depends_on")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            coordinator.add_task(
                actor,
                string("project"),
                string("title"),
                args.get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                &depends,
                request_id,
            )
        }
        "work_depend" => {
            coordinator.add_dependency(actor, integer("task_id"), integer("depends_on"))
        }
        "work_claim" => coordinator.claim_task(
            actor,
            integer("task_id"),
            args.get("ttl_secs")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_TTL_SECS),
            request_id,
        ),
        "work_claim_path" => coordinator.claim_path(
            actor,
            string("project"),
            string("path"),
            optional("task_id"),
            args.get("ttl_secs")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_TTL_SECS),
            request_id,
        ),
        "work_renew" => coordinator.renew(
            actor,
            integer("claim_id"),
            integer("fence"),
            args.get("ttl_secs")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_TTL_SECS),
        ),
        "work_release" => coordinator.release(
            actor,
            integer("claim_id"),
            integer("fence"),
            args.get("note").and_then(Value::as_str).unwrap_or_default(),
        ),
        "work_set_state" => coordinator.set_state(
            actor,
            integer("claim_id"),
            integer("fence"),
            string("state"),
            args.get("note").and_then(Value::as_str).unwrap_or_default(),
            request_id,
        ),
        "work_handoff" => coordinator.handoff(
            actor,
            integer("claim_id"),
            integer("fence"),
            string("to_agent"),
            string("to_machine"),
            args.get("ttl_secs")
                .and_then(Value::as_i64)
                .unwrap_or(DEFAULT_TTL_SECS),
            args.get("note").and_then(Value::as_str).unwrap_or_default(),
            request_id,
        ),
        "work_verify" => coordinator.verify(actor, integer("claim_id"), integer("fence")),
        _ => Err(CoordError {
            kind: ErrorKind::NotFound,
            message: "unknown tool".to_owned(),
            details: Value::Null,
        }),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::protocol;
    use std::fs;
    use std::net::TcpStream;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_state(tag: &str) -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("brain-net-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }

    fn raw_call(handle: &ServerHandle, request: &str) -> (u16, Value) {
        let stream = TcpStream::connect(("127.0.0.1", handle.port())).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut stream = stream;
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        // A refused request may be answered and closed while we are still
        // sending; a failed read then means "connection closed", not a panic.
        let _ = stream.read_to_string(&mut response);
        let status: u16 = response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = response
            .split_once("\r\n\r\n")
            .and_then(|(_, body)| serde_json::from_str(body).ok())
            .unwrap_or(json!({"error": "connection_closed"}));
        (status, body)
    }

    fn request_bytes(
        method: &str,
        path: &str,
        host: &str,
        token: Option<&str>,
        body: &str,
        extra: &[(&str, &str)],
    ) -> String {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
        if let Some(token) = token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (name, value) in extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        head
    }

    fn json_body(tool: &str, agent: &str, arguments: &str) -> String {
        format!("{{\"tool\":\"{tool}\",\"agent\":\"{agent}\",\"arguments\":{arguments}}}")
    }

    /// Starts a server that owns its accept thread; the returned handle stops
    /// and joins it on drop, so nothing leaks between tests.
    fn start_serving(tag: &str) -> (ServerHandle, Admin, TempDir) {
        let dir = temp_state(tag);
        let server = CoordServer::start(&ServerConfig::new(&dir.0, 0)).expect("start");
        let admin = Admin {
            inner: Arc::clone(server.inner()),
        };
        let handle = server.serve();
        // The accept loop is live once a connection succeeds.
        loop {
            if TcpStream::connect(("127.0.0.1", handle.port())).is_ok() {
                break;
            }
        }
        (handle, admin, dir)
    }

    /// Administration side channel while the server owns the accept thread.
    struct Admin {
        inner: Arc<Inner>,
    }

    impl Admin {
        fn upsert_and_token(
            &self,
            machine: &str,
            agent: &str,
            role: &str,
            spaces: &[&str],
        ) -> String {
            let owned: Vec<String> = spaces.iter().map(|space| (*space).to_owned()).collect();
            let state = self.inner.state.lock().expect("lock");
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            state
                .auth
                .upsert_principal(machine, agent, machine, role, &refs)
                .expect("principal");
            state
                .auth
                .issue_credential(machine, agent, None)
                .expect("credential")
                .as_str()
                .to_owned()
        }

        fn revoke(&self, token_id: &str) {
            self.inner
                .state
                .lock()
                .expect("lock")
                .auth
                .revoke_credential(token_id)
                .expect("revoke");
        }
    }

    #[test]
    fn unauthenticated_requests_are_uniformly_refused() {
        let (handle, _admin, _dir) = start_serving("unauth");
        let cases = [
            request_bytes(
                "POST",
                protocol::ENDPOINT,
                "127.0.0.1",
                None,
                &json_body("work_board", "a", r#"{"space":"s"}"#),
                &[],
            ),
            request_bytes(
                "POST",
                protocol::ENDPOINT,
                "127.0.0.1",
                Some("brc1.nope.wrong"),
                &json_body("work_board", "a", r#"{"space":"s"}"#),
                &[],
            ),
            request_bytes("POST", protocol::ENDPOINT, "127.0.0.1", None, "{}", &[]),
        ];
        let mut bodies = Vec::new();
        for request in cases {
            let (status, body) = raw_call(&handle, &request);
            assert_eq!(status, 401, "must be 401");
            bodies.push(body.to_string());
        }
        assert!(
            bodies.windows(2).all(|pair| pair[0] == pair[1]),
            "401 bodies must be byte-identical"
        );
    }

    #[test]
    fn token_in_query_string_is_refused() {
        let (handle, _admin, _dir) = start_serving("querytoken");
        let request = request_bytes(
            "POST",
            &format!("{}?token=brc1.t.abc", protocol::ENDPOINT),
            "127.0.0.1",
            None,
            "{}",
            &[],
        );
        let (status, _) = raw_call(&handle, &request);
        assert_eq!(status, 401);
    }

    #[test]
    fn reader_cannot_write_but_reads_the_board() {
        let (handle, admin, _dir) = start_serving("reader");
        let token = admin.upsert_and_token("m-reader", "a", "reader", &["team-a"]);
        let read = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body("work_board", "a", r#"{"space":"team-a"}"#),
            &[],
        );
        let (status, body) = raw_call(&handle, &read);
        assert_eq!(status, 200, "reader may read");
        assert_eq!(body["result"]["status"], "ok");
        let write = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_add",
                "a",
                r#"{"space":"team-a","project":"p","title":"t"}"#,
            ),
            &[],
        );
        let (status, body) = raw_call(&handle, &write);
        assert_eq!(status, 403, "reader may not write: {body}");
    }

    #[test]
    fn space_allowlist_is_enforced_before_schema() {
        let (handle, admin, _dir) = start_serving("spaces");
        let token = admin.upsert_and_token("m-w1", "a", "worker", &["team-a"]);
        let foreign = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_add",
                "a",
                r#"{"space":"team-b","project":"p","title":"t","unknown_field":1}"#,
            ),
            &[],
        );
        let (status, _) = raw_call(&handle, &foreign);
        assert_eq!(status, 403, "authorization precedes schema");
        let own = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_add",
                "a",
                r#"{"space":"team-a","project":"p","title":"t"}"#,
            ),
            &[],
        );
        let (status, body) = raw_call(&handle, &own);
        assert_eq!(status, 200, "own space write works: {body}");
        let task_id = body["result"]["task"]["id"].as_i64().expect("task id");
        assert!(task_id > 0);
    }

    #[test]
    fn origin_and_bad_host_are_refused() {
        let (handle, admin, _dir) = start_serving("origin");
        let token = admin.upsert_and_token("m-o", "a", "worker", &["s"]);
        let origin = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body("work_board", "a", r#"{"space":"s"}"#),
            &[("Origin", "http://evil.example")],
        );
        let (status, _) = raw_call(&handle, &origin);
        assert_eq!(status, 403);
        let bad_host = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "evil.example",
            Some(&token),
            &json_body("work_board", "a", r#"{"space":"s"}"#),
            &[],
        );
        let (status, _) = raw_call(&handle, &bad_host);
        assert_eq!(status, 403);
        let get = request_bytes(
            "GET",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            "",
            &[],
        );
        let (status, _) = raw_call(&handle, &get);
        assert_eq!(status, 405);
        let other_path = request_bytes("POST", "/v1/admin", "127.0.0.1", Some(&token), "{}", &[]);
        let (status, _) = raw_call(&handle, &other_path);
        assert_eq!(status, 404);
    }

    #[test]
    fn machine_cannot_be_spoofed_from_the_body() {
        let (handle, admin, _dir) = start_serving("spoof");
        let token = admin.upsert_and_token("m-real", "a", "worker", &["s"]);
        let add = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_add",
                "a",
                r#"{"space":"s","project":"p","title":"t"}"#,
            ),
            &[],
        );
        let (_, body) = raw_call(&handle, &add);
        let task_id = body["result"]["task"]["id"].as_i64().expect("task");
        let claim = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_claim",
                "a",
                &format!(r#"{{"space":"s","task_id":{task_id},"machine":"m-fake"}}"#),
            ),
            &[],
        );
        let (status, body) = raw_call(&handle, &claim);
        assert_eq!(status, 400, "identity fields are refused: {body}");
        // A legitimate claim names the token's machine.
        let claim = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_claim",
                "a",
                &format!(r#"{{"space":"s","task_id":{task_id}}}"#),
            ),
            &[],
        );
        let (status, body) = raw_call(&handle, &claim);
        assert_eq!(status, 200);
        assert_eq!(
            body["result"]["claim"]["machine"], "m-real",
            "owner is the credential machine"
        );
    }

    #[test]
    fn oversized_body_and_headers_are_refused() {
        let (handle, admin, _dir) = start_serving("oversize");
        let token = admin.upsert_and_token("m-big", "a", "worker", &["s"]);
        let big = "x".repeat(protocol::MAX_BODY_BYTES + 1);
        let request = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &big,
            &[],
        );
        let (status, _) = raw_call(&handle, &request);
        assert_eq!(status, 413);
        let huge_header = "v".repeat(protocol::MAX_HEADER_BYTES + 100);
        let request = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body("work_board", "a", r#"{"space":"s"}"#),
            &[("X-Junk", &huge_header)],
        );
        let (status, _) = raw_call(&handle, &request);
        assert!(
            status == 413 || status == 431 || status == 0,
            "header flood must fail or close the connection, got {status}"
        );
    }

    #[test]
    fn revoked_credential_fails_on_the_next_request() {
        let (handle, admin, _dir) = start_serving("revoke");
        let token = admin.upsert_and_token("m-r", "a", "worker", &["s"]);
        let read = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body("work_board", "a", r#"{"space":"s"}"#),
            &[],
        );
        let (status, _) = raw_call(&handle, &read);
        assert_eq!(status, 200);
        let token_id = token.split('.').nth(1).expect("token id");
        admin.revoke(token_id);
        let (status, _) = raw_call(&handle, &read);
        assert_eq!(status, 401, "revocation is immediate");
        // Rotation: a second credential for the same machine still works.
        let state = admin.inner.state.lock().expect("lock");
        let second = state
            .auth
            .issue_credential("m-r", "a", None)
            .expect("second")
            .as_str()
            .to_owned();
        drop(state);
        let read = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&second),
            &json_body("work_board", "a", r#"{"space":"s"}"#),
            &[],
        );
        let (status, _) = raw_call(&handle, &read);
        assert_eq!(status, 200);
    }

    #[test]
    fn two_clients_race_one_task_exactly_one_wins() {
        let (handle, admin, _dir) = start_serving("race");
        let alice = admin.upsert_and_token("m-alice", "w", "worker", &["s"]);
        let bob = admin.upsert_and_token("m-bob", "w", "worker", &["s"]);
        let add = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&alice),
            &json_body(
                "work_add",
                "w",
                r#"{"space":"s","project":"p","title":"race"}"#,
            ),
            &[],
        );
        let (_, body) = raw_call(&handle, &add);
        let task_id = body["result"]["task"]["id"].as_i64().expect("task");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let claims: Vec<_> = [alice.clone(), bob.clone()]
            .into_iter()
            .map(|token| {
                let (server_port, barrier) = (handle.port(), Arc::clone(&barrier));
                let request = request_bytes(
                    "POST",
                    protocol::ENDPOINT,
                    "127.0.0.1",
                    Some(&token),
                    &json_body(
                        "work_claim",
                        "w",
                        &format!(r#"{{"space":"s","task_id":{task_id}}}"#),
                    ),
                    &[],
                );
                thread::spawn(move || {
                    barrier.wait();
                    let stream = TcpStream::connect(("127.0.0.1", server_port)).expect("connect");
                    call_raw(stream, &request)
                })
            })
            .collect();
        let outcomes = claims
            .into_iter()
            .map(|handle| handle.join().expect("join"))
            .collect::<Vec<_>>();
        let ok = outcomes
            .iter()
            .filter(|(_, body)| body["result"]["status"] == "ok")
            .count();
        let busy = outcomes
            .iter()
            .filter(|(_, body)| {
                body["result"]["status"] == "error" && body["result"]["error_kind"] == "busy"
            })
            .count();
        assert_eq!((ok, busy), (1, 1), "outcomes: {outcomes:?}");
    }

    fn call_raw(mut stream: TcpStream, request: &str) -> (u16, Value) {
        use std::io::Write as _;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        let status: u16 = response
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| serde_json::from_str(body).expect("json"))
            .expect("body");
        (status, body)
    }

    #[test]
    fn idempotent_replay_with_the_same_request_id() {
        let (handle, admin, _dir) = start_serving("replay");
        let token = admin.upsert_and_token("m-play", "a", "worker", &["s"]);
        let add = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body(
                "work_add",
                "a",
                r#"{"space":"s","project":"p","title":"replay"}"#,
            ),
            &[],
        );
        let (_, seeded) = raw_call(&handle, &add);
        let task_id = seeded["result"]["task"]["id"].as_i64().expect("seed task");
        let body = format!(r#"{{"space":"s","task_id":{task_id},"ttl_secs":600}}"#);
        let first = format!(
            r#"{{"tool":"work_claim","agent":"a","arguments":{body},"request_id":"fixed-1"}}"#
        );
        let request = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &first,
            &[],
        );
        let (status, first_body) = raw_call(&handle, &request);
        assert_eq!(status, 200);
        let claim = first_body["result"]["claim"]["claim_id"]
            .as_i64()
            .expect("claim");
        let (status, second_body) = raw_call(&handle, &request);
        assert_eq!(status, 200);
        assert_eq!(
            second_body["result"]["claim"]["claim_id"], claim,
            "same request replays the same claim"
        );
        // Same body shape, so this is a replay too; a different body must conflict.
        let conflicting = format!(
            r#"{{"tool":"work_claim","agent":"a","arguments":{{"space":"s","task_id":{task_id},"ttl_secs":300}},"request_id":"fixed-1"}}"#
        );
        let request = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &conflicting,
            &[],
        );
        let (status, body) = raw_call(&handle, &request);
        assert_eq!(status, 200);
        assert_eq!(
            body["result"]["error_kind"], "idempotency_conflict",
            "different body with same id: {body}"
        );
    }

    #[test]
    fn whoami_reports_the_credential_bound_agent() {
        let (handle, admin, _dir) = start_serving("whoami");
        let token = admin.upsert_and_token("m-w", "codex", "worker", &["s"]);
        let named = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            &json_body("work_whoami", "codex", "{}"),
            &[],
        );
        let (status, body) = raw_call(&handle, &named);
        assert_eq!(status, 200);
        assert_eq!(
            body["principal"]["agent"], "codex",
            "whoami reports the bound agent: {body}"
        );
        // An omitted agent is derived from the credential, never guessed.
        let omitted = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&token),
            r#"{"tool":"work_whoami","arguments":{}}"#,
            &[],
        );
        let (status, body) = raw_call(&handle, &omitted);
        assert_eq!(status, 200, "omitted agent is derived: {body}");
        assert_eq!(body["principal"]["agent"], "codex");
    }

    #[test]
    fn second_agent_on_the_same_machine_cannot_act_as_the_first() {
        let (handle, admin, _dir) = start_serving("binding");
        // One machine, two agents, two valid credentials.
        let codex = admin.upsert_and_token("m-shared", "codex", "worker", &["s"]);
        let claude = admin.upsert_and_token("m-shared", "claude", "worker", &["s"]);
        let add = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&codex),
            &json_body(
                "work_add",
                "codex",
                r#"{"space":"s","project":"p","title":"binding"}"#,
            ),
            &[],
        );
        let (_, body) = raw_call(&handle, &add);
        let task_id = body["result"]["task"]["id"].as_i64().expect("task");
        let args = format!(r#"{{"space":"s","task_id":{task_id},"ttl_secs":600}}"#);
        let claim_req = format!(
            r#"{{"tool":"work_claim","agent":"codex","arguments":{args},"request_id":"codex-1"}}"#
        );
        let claim = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&codex),
            &claim_req,
            &[],
        );
        let (status, claimed) = raw_call(&handle, &claim);
        assert_eq!(status, 200);
        assert_eq!(claimed["result"]["status"], "ok", "codex claims: {claimed}");
        let claim_id = claimed["result"]["claim"]["claim_id"]
            .as_i64()
            .expect("claim id");
        let fence = claimed["result"]["claim"]["fence"].as_i64().expect("fence");

        // A borrowed agent label is refused outright.
        let spoof = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&claude),
            &json_body("work_board", "codex", r#"{"space":"s"}"#),
            &[],
        );
        let (status, body) = raw_call(&handle, &spoof);
        assert_eq!(status, 403, "claude may not name codex: {body}");

        // Knowing claim_id and fence does not let claude renew codex's claim.
        let renew = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&claude),
            &json_body(
                "work_renew",
                "claude",
                &format!(r#"{{"space":"s","claim_id":{claim_id},"fence":{fence}}}"#),
            ),
            &[],
        );
        let (status, body) = raw_call(&handle, &renew);
        assert_eq!(status, 200);
        assert_eq!(
            body["result"]["error_kind"], "not_owner",
            "renew is bound to the credential agent: {body}"
        );

        // Idempotency cannot cross credentials: the same request_id under a
        // different identity is a fresh call, never the stored answer.
        let replay_req = format!(
            r#"{{"tool":"work_claim","agent":"claude","arguments":{args},"request_id":"codex-1"}}"#
        );
        let replay = request_bytes(
            "POST",
            protocol::ENDPOINT,
            "127.0.0.1",
            Some(&claude),
            &replay_req,
            &[],
        );
        let (status, body) = raw_call(&handle, &replay);
        assert_eq!(status, 200);
        assert_eq!(
            body["result"]["error_kind"], "busy",
            "no replay across agents: {body}"
        );

        // Codex's own replay still returns his stored claim.
        let (status, body) = raw_call(&handle, &claim);
        assert_eq!(status, 200);
        assert_eq!(
            body["result"]["claim"]["claim_id"], claim_id,
            "same credential replays the same claim"
        );
    }
}
