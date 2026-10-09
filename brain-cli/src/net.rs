//! `brain net …`: the `brain-coord/1` coordinator over loopback TLS.
//!
//! * `serve` runs a TLS coordinator bound to `127.0.0.1` only. There is no
//!   bind option: a LAN listener is a separate deployment gate.
//! * `enroll`, `revoke`, `principals` are trusted LOCAL administration of
//!   the coordinator's own state directory; they never touch the network.
//! * `status` and `call` are the client: a numeric loopback dial target,
//!   a separate certificate name, a pinned CA file and a credential FILE
//!   or inherited descriptor. A token is never accepted on the command
//!   line, never printed, never put in an error.
//!
//! Identity comes from the credential. The global `--agent` is not sent:
//! the server derives the acting agent and machine from the token.
//! Every command prints exactly one JSON object; the exit code says what
//! happened (see [`Exit`]). Nothing here writes Brain's user config or
//! database; every path is explicit.

use std::error::Error;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::network::auth::{DEFAULT_CREDENTIAL_TTL_SECS, MIN_CREDENTIAL_TTL_SECS};
use brain_core::network::tls::{MAX_DEADLINE, MIN_DEADLINE};
use brain_core::network::{
    AuthStore, KeySource, NetError, TcpDialer, TlsCallResult, TlsClientConfig, TlsCoordClient,
    TlsCoordServer, TlsError, TlsServerConfig, allowed_tools, parse_dial_target_lan,
    tools_for_role, write_credential_file,
};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::RuntimeConfig;

mod share;

const PROTOCOL: &str = "brain-coord/1";

/// Exit codes. Every outcome also prints one JSON object with `status`.
#[derive(Clone, Copy)]
enum Exit {
    Ok = 0,
    InvalidConfig = 2,
    /// The coordinator answered; the domain said no (busy, conflict, stale fence).
    DomainError = 3,
    /// The coordinator refused the request (401/403/400/404/413) or admin `not_found`.
    Refused = 4,
    /// No TLS session: wrong CA, wrong name, expired certificate, not TLS.
    TlsFailure = 5,
    /// Nothing reached the coordinator (connect failed / deadline before the handshake).
    Unreachable = 6,
    /// The request may have been applied; see `retry` in the output.
    UnknownOutcome = 7,
}

#[derive(Debug, Subcommand)]
pub(crate) enum NetCommand {
    /// Serve the coordinator over TLS (127.0.0.1 by default) until standard input closes.
    ///
    /// Prints one JSON line with the actual bound address once it accepts.
    /// A private LAN address needs both --listen and the explicit --allow-lan.
    Serve(ServeArgs),
    /// Trusted local admin: create/refresh a principal and write a new credential file (0600).
    ///
    /// The token goes to --credential-out only; stdout shows its token_id.
    Enroll(EnrollArgs),
    /// Trusted local admin: revoke one credential, or disable a machine and all its credentials.
    Revoke(RevokeArgs),
    /// Trusted local admin: list principals (no secrets, no credential hashes).
    Principals(StateDirArg),
    /// Ask the coordinator who this credential is and what it may call.
    Status(ConnectArgs),
    /// Opt-in shared memory: host-local enable/grant/publish and TLS pull.
    Share {
        #[command(subcommand)]
        command: share::ShareCommand,
    },
    /// Call one allowlisted coordinator tool over TLS.
    Call {
        #[command(flatten)]
        connect: ConnectArgs,
        #[command(subcommand)]
        operation: RemoteOp,
    },
}

#[derive(Debug, Args)]
pub(crate) struct StateDirArg {
    /// Coordinator state directory (auth.sqlite, coord.sqlite). Required; no default.
    #[arg(long, value_name = "DIR")]
    state_dir: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
    #[command(flatten)]
    state: StateDirArg,
    /// Server certificate chain (PEM, owned by this uid).
    #[arg(long, value_name = "FILE")]
    cert: PathBuf,
    /// Server private key (PEM, 0600, no symlink).
    #[arg(
        long,
        value_name = "FILE",
        required_unless_present = "key_fd",
        conflicts_with = "key_fd"
    )]
    key: Option<PathBuf>,
    /// Inherited descriptor (>= 3) holding the private key; same checks as --key.
    #[arg(long, value_name = "N")]
    key_fd: Option<i32>,
    /// Port; 0 picks a free one (the JSON line says which).
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Numeric listen IP (default 127.0.0.1). Hostnames are refused, never resolved.
    #[arg(long, value_name = "IP", default_value = "127.0.0.1")]
    listen: String,
    /// Opt in to listening on a private unicast LAN address (RFC 1918 or fc00::/7).
    /// Wildcard, multicast, broadcast, link-local and public addresses are always refused.
    #[arg(long)]
    allow_lan: bool,
    /// Absolute per-connection deadline in milliseconds (50..=5000).
    #[arg(long, default_value_t = 5000)]
    deadline_ms: u64,
}

#[derive(Debug, Args)]
pub(crate) struct EnrollArgs {
    #[command(flatten)]
    state: StateDirArg,
    /// Machine (installation) id, [a-z0-9-]{3,64}.
    #[arg(long)]
    machine: String,
    /// Agent id bound to the credential, [A-Za-z0-9._:@-]{1,64}.
    #[arg(long)]
    agent_id: String,
    /// worker (read and write) or reader (read only).
    #[arg(long, value_parser = ["worker", "reader"])]
    role: String,
    /// Space this principal may use (repeatable, at least one).
    #[arg(long = "grant-space", required = true)]
    grant_space: Vec<String>,
    /// Informational label, never an identity.
    #[arg(long, default_value = "")]
    label: String,
    /// Credential lifetime in seconds (60..=7776000, default 90 days).
    #[arg(long)]
    ttl_secs: Option<i64>,
    /// New file for the credential; must not exist. Created 0600.
    #[arg(long, value_name = "FILE")]
    credential_out: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct RevokeArgs {
    #[command(flatten)]
    state: StateDirArg,
    /// Revoke exactly this credential.
    #[arg(long, required_unless_present = "machine", conflicts_with = "machine")]
    token_id: Option<String>,
    /// Disable this machine: every agent and credential on it.
    #[arg(long)]
    machine: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ConnectArgs {
    /// Numeric dial target, e.g. 127.0.0.1:7443. Hostnames are refused, never resolved.
    #[arg(long, value_name = "IP:PORT")]
    dial: String,
    /// Opt in to dialing a private unicast LAN address (same policy as --allow-lan).
    #[arg(long)]
    allow_lan_dial: bool,
    /// Name the server certificate must carry (SAN), e.g. localhost.
    #[arg(long, value_name = "NAME")]
    server_name: String,
    /// The only CA trusted for this call (PEM). System roots are never used.
    #[arg(long, value_name = "FILE")]
    ca: PathBuf,
    /// Credential file (0600, no symlink, owned by this uid).
    #[arg(
        long,
        value_name = "FILE",
        required_unless_present = "credential_fd",
        conflicts_with = "credential_fd"
    )]
    credential: Option<PathBuf>,
    /// Inherited descriptor (>= 3) of a credential file; same checks as --credential.
    #[arg(long, value_name = "N")]
    credential_fd: Option<i32>,
    /// Absolute deadline for connect + handshake + request + response (50..=5000 ms).
    #[arg(long, default_value_t = 5000)]
    deadline_ms: u64,
}

/// The typed client surface: each variant maps to ONE allowlisted tool
/// with a fixed argument schema. There is no free-form tool name.
#[derive(Debug, Subcommand)]
pub(crate) enum RemoteOp {
    /// The credential's identity as the coordinator sees it.
    Whoami,
    /// One page of tasks with their live owners.
    Board {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, value_parser = ["queued", "active", "paused", "completed", "failed"])]
        state: Option<String>,
        #[arg(long)]
        ready: bool,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: u32,
    },
    /// One task.
    Show {
        #[arg(value_parser = positive())]
        task: i64,
    },
    /// Live claims.
    Claims {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: u32,
    },
    /// Add a queued task. Safe to retry with the same --request-id.
    Add {
        project: String,
        title: String,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long = "depends-on", value_parser = positive())]
        depends_on: Vec<i64>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// TASK depends on ON. Not idempotent by id: query before retrying.
    Depend {
        #[arg(value_parser = positive())]
        task: i64,
        #[arg(value_parser = positive())]
        on: i64,
    },
    /// Claim a task. Safe to retry with the same --request-id.
    Claim {
        #[arg(value_parser = positive())]
        task: i64,
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Claim a path. Safe to retry with the same --request-id.
    ClaimPath {
        project: String,
        path: String,
        #[arg(long, value_parser = positive())]
        task: Option<i64>,
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Extend a lease. No request id: on an unknown outcome, query `verify` first.
    Renew {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
    },
    /// Give a claim up. No request id: on an unknown outcome, query `verify` first.
    Release {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        #[arg(long)]
        note: Option<String>,
    },
    /// Move a claimed task to paused/completed/failed/queued (finish). Safe to retry with the same --request-id.
    State {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        #[arg(value_parser = ["queued", "paused", "completed", "failed"])]
        state: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Pass a live claim on. Safe to retry with the same --request-id.
    Handoff {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        #[arg(long)]
        to_agent: String,
        #[arg(long)]
        to_machine: String,
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Read one page of the host's own shared events in --space (needs a host memory grant).
    SharePull {
        #[arg(long, default_value_t = 0)]
        after_seq: u64,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=10))]
        limit: u32,
    },
    /// Live facts the host itself published in --space (needs a host memory grant).
    ShareQuery {
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=10))]
        limit: u32,
    },
    /// Exit 0 only if CLAIM/FENCE is still this credential's live claim.
    Verify {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
    },
}

fn positive() -> clap::builder::RangedI64ValueParser<i64> {
    clap::value_parser!(i64).range(1..)
}

/// How a caller may retry when the outcome is unknown.
enum Retry {
    /// A read: rerunning is harmless.
    Read,
    /// The coordinator deduplicates by this request id.
    SameRequestId(String),
    /// No request id on the wire: query state before acting again.
    QueryFirst(&'static str),
}

pub(crate) fn run(config: &RuntimeConfig, command: NetCommand) -> Result<(), Box<dyn Error>> {
    let (exit, document) = match command {
        NetCommand::Serve(args) => return serve(&args),
        NetCommand::Enroll(args) => enroll(&args),
        NetCommand::Revoke(args) => revoke(&args),
        NetCommand::Principals(args) => principals(&args),
        NetCommand::Status(connect) => status(&connect),
        NetCommand::Call { connect, operation } => call(config, &connect, operation),
        NetCommand::Share { command } => share::run(command),
    };
    finish(exit, &document)
}

fn finish(exit: Exit, document: &Value) -> Result<(), Box<dyn Error>> {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", serde_json::to_string_pretty(document)?)?;
    stdout.flush()?;
    match exit {
        Exit::Ok => Ok(()),
        other => std::process::exit(other as i32),
    }
}

fn failure(status: &str, code: &str, message: &str) -> Value {
    json!({"status": status, "error": {"code": code, "message": message}})
}

fn invalid(message: impl AsRef<str>) -> (Exit, Value) {
    (
        Exit::InvalidConfig,
        failure("invalid_config", "invalid_config", message.as_ref()),
    )
}

fn deadline(ms: u64) -> Result<Duration, (Exit, Value)> {
    let deadline = Duration::from_millis(ms);
    if (MIN_DEADLINE..=MAX_DEADLINE).contains(&deadline) {
        Ok(deadline)
    } else {
        Err(invalid(format!(
            "--deadline-ms must be {}..={}",
            MIN_DEADLINE.as_millis(),
            MAX_DEADLINE.as_millis()
        )))
    }
}

/// `NetError` text is built by this crate and never holds secret bytes.
fn net_error(error: &NetError) -> Value {
    json!({"http_status": error.status(), "detail": error.body()})
}

// ---- serve ---------------------------------------------------------------

fn serve(args: &ServeArgs) -> Result<(), Box<dyn Error>> {
    let deadline = match deadline(args.deadline_ms) {
        Ok(deadline) => deadline,
        Err((exit, document)) => return finish(exit, &document),
    };
    let key = match (&args.key, args.key_fd) {
        (Some(path), None) => KeySource::File(path.clone()),
        (None, Some(fd)) => KeySource::Fd(fd),
        _ => {
            return finish(
                Exit::InvalidConfig,
                &invalid("exactly one of --key / --key-fd").1,
            );
        }
    };
    let Ok(listen) = args.listen.parse::<std::net::IpAddr>() else {
        return finish(
            Exit::InvalidConfig,
            &invalid("--listen must be a numeric IP; hostnames are never resolved").1,
        );
    };
    let mut config =
        TlsServerConfig::new(&args.state.state_dir, args.port, &args.cert, PathBuf::new());
    config.key = key;
    config.deadline = deadline;
    config.listen = listen;
    config.allow_lan = args.allow_lan;
    let server = match TlsCoordServer::start(&config) {
        Ok(server) => server,
        Err(error) => {
            let exit = if error.status() == 400 {
                Exit::InvalidConfig
            } else {
                Exit::Unreachable
            };
            return finish(
                exit,
                &json!({"status": "not_serving", "error": net_error(&error)}),
            );
        }
    };
    let handle = match server.serve() {
        Ok(handle) => handle,
        Err(error) => {
            return finish(
                Exit::Unreachable,
                &json!({"status": "not_serving", "error": net_error(&error)}),
            );
        }
    };
    // One compact line, so a supervisor can read it before the server stops.
    let ready = json!({
        "status": "serving",
        "protocol": PROTOCOL,
        "transport": "tls (rustls; TLS 1.2/1.3)",
        "bind": std::net::SocketAddr::new(listen, handle.port()).to_string(),
        "lan": !listen.is_loopback(),
        "state_dir": args.state.state_dir,
        "deadline_ms": deadline.as_millis() as u64,
        "stop": "close standard input",
    });
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{ready}")?;
        stdout.flush()?;
    }
    // Owned stop: serve until stdin reaches EOF (or the process is killed).
    let mut sink = [0_u8; 256];
    let mut stdin = std::io::stdin().lock();
    while matches!(stdin.read(&mut sink), Ok(read) if read > 0) {}
    let joined = handle.stop();
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "{}",
        json!({"status": "stopped", "joined_connections": joined})
    )?;
    stdout.flush()?;
    Ok(())
}

// ---- local administration ------------------------------------------------

fn existing_store(state_dir: &Path) -> Result<AuthStore, (Exit, Value)> {
    if !state_dir.join("auth.sqlite").is_file() {
        return Err((
            Exit::InvalidConfig,
            failure(
                "not_configured",
                "not_configured",
                "no auth.sqlite in --state-dir; enroll a principal first",
            ),
        ));
    }
    AuthStore::open(state_dir).map_err(|error| {
        (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error)}),
        )
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn enroll(args: &EnrollArgs) -> (Exit, Value) {
    let ttl = args.ttl_secs.unwrap_or(DEFAULT_CREDENTIAL_TTL_SECS);
    if !(MIN_CREDENTIAL_TTL_SECS..=DEFAULT_CREDENTIAL_TTL_SECS).contains(&ttl) {
        return invalid(format!(
            "--ttl-secs must be {MIN_CREDENTIAL_TTL_SECS}..={DEFAULT_CREDENTIAL_TTL_SECS}"
        ));
    }
    // Refuse before issuing anything, so a bad path never strands a token.
    let out = &args.credential_out;
    if out.symlink_metadata().is_ok() {
        return invalid("--credential-out already exists; credentials are never overwritten");
    }
    if !out
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty() || parent.is_dir())
    {
        return invalid("the directory of --credential-out must exist");
    }
    let store = match AuthStore::open(&args.state.state_dir) {
        Ok(store) => store,
        Err(error) => {
            return (
                Exit::InvalidConfig,
                json!({"status": "invalid_config", "error": net_error(&error)}),
            );
        }
    };
    let spaces: Vec<&str> = args.grant_space.iter().map(String::as_str).collect();
    if let Err(error) = store.upsert_principal(
        &args.machine,
        &args.agent_id,
        &args.label,
        &args.role,
        &spaces,
    ) {
        return (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error)}),
        );
    }
    let issued_at = unix_now();
    let token = match store.issue_credential(&args.machine, &args.agent_id, Some(ttl)) {
        Ok(token) => token,
        Err(error) => {
            return (
                Exit::InvalidConfig,
                json!({"status": "invalid_config", "error": net_error(&error)}),
            );
        }
    };
    let token_id = token.token_id().as_str().to_owned();
    if let Err(error) = write_credential_file(out, token.as_str()) {
        // The secret never reached a file: make sure it can never be used.
        let _ = store.revoke_credential(&token_id);
        return (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error), "revoked_token_id": token_id}),
        );
    }
    drop(token);
    (
        Exit::Ok,
        json!({
            "status": "ok",
            "principal": {"machine": args.machine, "agent": args.agent_id, "role": args.role, "spaces": args.grant_space, "label": args.label},
            "token_id": token_id,
            "expires_at": issued_at + ttl,
            "credential_file": out,
            "secret": "written to credential_file (0600) only",
            "capabilities": tools_for_role(&args.role),
        }),
    )
}

fn revoke(args: &RevokeArgs) -> (Exit, Value) {
    let store = match existing_store(&args.state.state_dir) {
        Ok(store) => store,
        Err(failed) => return failed,
    };
    let (target, outcome) = match (&args.token_id, &args.machine) {
        (Some(token_id), None) => (
            json!({"token_id": token_id}),
            store.revoke_credential(token_id),
        ),
        (None, Some(machine)) => (
            json!({"machine": machine, "all_agents_and_credentials": true}),
            store.disable_principal(machine),
        ),
        _ => return invalid("exactly one of --token-id / --machine"),
    };
    match outcome {
        Ok(()) => (
            Exit::Ok,
            json!({"status": "ok", "revoked": target, "effective": "next request; no cached replay is served"}),
        ),
        Err(error) => (
            Exit::Refused,
            json!({"status": "not_found", "target": target, "error": net_error(&error)}),
        ),
    }
}

fn principals(args: &StateDirArg) -> (Exit, Value) {
    let store = match existing_store(&args.state_dir) {
        Ok(store) => store,
        Err(failed) => return failed,
    };
    match store.list_principals(200) {
        Ok(list) => (
            Exit::Ok,
            json!({"status": "ok", "principals": list, "limit": 200}),
        ),
        Err(error) => (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error)}),
        ),
    }
}

// ---- client ----------------------------------------------------------------

fn client(connect: &ConnectArgs) -> Result<TlsCoordClient, (Exit, Value)> {
    let deadline = deadline(connect.deadline_ms)?;
    let dial = parse_dial_target_lan(&connect.dial, connect.allow_lan_dial)
        .map_err(|error| invalid(tls_message(&error)))?;
    let credential = match (&connect.credential, connect.credential_fd) {
        (Some(path), None) => KeySource::File(path.clone()),
        (None, Some(fd)) => KeySource::Fd(fd),
        _ => return Err(invalid("exactly one of --credential / --credential-fd")),
    };
    let mut config = TlsClientConfig::new(&connect.ca, &connect.server_name, dial);
    config.deadline = deadline;
    config.allow_lan = connect.allow_lan_dial;
    TlsCoordClient::with_dialer(config, &credential, Arc::new(TcpDialer)).map_err(|error| {
        (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error)}),
        )
    })
}

fn tls_message(error: &TlsError) -> String {
    error.to_string()
}

fn transport(client: &TlsCoordClient, connect: &ConnectArgs) -> Value {
    json!({
        "protocol": PROTOCOL,
        "tls": true,
        "dial": client.dial_target().to_string(),
        "server_name": client.server_name(),
        "pinned_ca": connect.ca,
        "system_roots": false,
    })
}

fn status(connect: &ConnectArgs) -> (Exit, Value) {
    let client = match client(connect) {
        Ok(client) => client,
        Err(failed) => return failed,
    };
    let endpoint = transport(&client, connect);
    let outcome = client.call("work_whoami", "", Map::new(), None);
    let (exit, mut document) = outcome_document(outcome, &Retry::Read);
    document["endpoint"] = endpoint;
    if let Some(principal) = document.pointer("/response/principal").cloned() {
        let role = principal
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        document["principal"] = principal.clone();
        document["capabilities"] = json!(tools_for_role(role));
    }
    document["not_offered_here"] = json!([
        "binding a LAN interface (separate deployment gate)",
        "private memory or archive access (only explicitly published shared facts, via share_pull/share_query with a host grant)",
        "automatic retries (the caller decides; see retry in call output)",
    ]);
    (exit, document)
}

fn call(config: &RuntimeConfig, connect: &ConnectArgs, operation: RemoteOp) -> (Exit, Value) {
    let (tool, arguments, retry) = match request_for(&config.space, operation) {
        Ok(request) => request,
        Err(failed) => return failed,
    };
    // Defence in depth: the typed surface can only name allowlisted tools.
    if !allowed_tools().contains(&tool) {
        return invalid("tool is not allowlisted");
    }
    let client = match client(connect) {
        Ok(client) => client,
        Err(failed) => return failed,
    };
    let request_id = match &retry {
        Retry::SameRequestId(id) => Some(id.clone()),
        _ => None,
    };
    let endpoint = transport(&client, connect);
    let outcome = client.call(tool, "", arguments, request_id.clone());
    let (exit, mut document) = outcome_document(outcome, &retry);
    document["tool"] = json!(tool);
    document["endpoint"] = endpoint;
    if let Some(id) = request_id {
        document["request_id"] = json!(id);
    }
    (exit, document)
}

fn outcome_document(outcome: TlsCallResult, retry: &Retry) -> (Exit, Value) {
    match outcome {
        TlsCallResult::Ok(body) => {
            let domain_ok = body
                .get("result")
                .is_none_or(|result| result.get("status") != Some(&json!("error")));
            if domain_ok {
                (Exit::Ok, json!({"status": "ok", "response": body}))
            } else {
                (
                    Exit::DomainError,
                    json!({"status": "domain_error", "response": body}),
                )
            }
        }
        TlsCallResult::Refused(error) => (
            Exit::Refused,
            json!({"status": "refused", "error": net_error(&error)}),
        ),
        TlsCallResult::Tls(TlsError::Unreachable(message)) => (
            Exit::Unreachable,
            json!({"status": "unreachable", "error": {"code": "unreachable", "message": message}, "sent": false}),
        ),
        TlsCallResult::Tls(error) => (
            Exit::TlsFailure,
            json!({"status": "tls_failure", "error": {"code": error.code(), "message": tls_message(&error)}, "sent": false}),
        ),
        TlsCallResult::Unreachable(message) => {
            let advice = match retry {
                Retry::Read => json!({"safe": true, "how": "rerun the same read"}),
                Retry::SameRequestId(id) => {
                    json!({"safe": true, "how": "rerun with the same --request-id", "request_id": id})
                }
                Retry::QueryFirst(query) => {
                    json!({"safe": false, "how": format!("query `{query}` before acting again")})
                }
            };
            (
                Exit::UnknownOutcome,
                json!({"status": "unknown_outcome", "error": {"code": "unknown_outcome", "message": message}, "retry": advice}),
            )
        }
    }
}

/// A fresh request id for an idempotent write the caller did not name.
fn fresh_request_id() -> Result<String, (Exit, Value)> {
    let mut bytes = [0_u8; 12];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|_| invalid("cannot read /dev/urandom for a request id"))?;
    Ok(format!(
        "cli-{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn request_id(given: Option<String>) -> Result<Retry, (Exit, Value)> {
    Ok(Retry::SameRequestId(match given {
        Some(id) => id,
        None => fresh_request_id()?,
    }))
}

type Request = (&'static str, Map<String, Value>, Retry);

/// Maps one typed operation to its allowlisted tool and exact arguments.
fn request_for(space: &str, operation: RemoteOp) -> Result<Request, (Exit, Value)> {
    let mut args = Map::new();
    let mut put = |key: &str, value: Value| {
        args.insert(key.to_owned(), value);
    };
    let (tool, retry) = match operation {
        RemoteOp::Whoami => return Ok(("work_whoami", Map::new(), Retry::Read)),
        RemoteOp::Board {
            project,
            state,
            ready,
            offset,
            limit,
        } => {
            put("space", json!(space));
            if let Some(project) = project {
                put("project", json!(project));
            }
            if let Some(state) = state {
                put("state", json!(state));
            }
            if ready {
                put("ready", json!(true));
            }
            put("offset", json!(offset));
            put("limit", json!(limit));
            ("work_board", Retry::Read)
        }
        RemoteOp::Show { task } => {
            put("space", json!(space));
            put("task_id", json!(task));
            ("work_show", Retry::Read)
        }
        RemoteOp::Claims {
            project,
            offset,
            limit,
        } => {
            put("space", json!(space));
            if let Some(project) = project {
                put("project", json!(project));
            }
            put("offset", json!(offset));
            put("limit", json!(limit));
            ("work_claims", Retry::Read)
        }
        RemoteOp::Add {
            project,
            title,
            detail,
            depends_on,
            request_id: id,
        } => {
            put("space", json!(space));
            put("project", json!(project));
            put("title", json!(title));
            if let Some(detail) = detail {
                put("detail", json!(detail));
            }
            if !depends_on.is_empty() {
                put("depends_on", json!(depends_on));
            }
            ("work_add", request_id(id)?)
        }
        RemoteOp::Depend { task, on } => {
            put("space", json!(space));
            put("task_id", json!(task));
            put("depends_on", json!(on));
            ("work_depend", Retry::QueryFirst("show TASK"))
        }
        RemoteOp::Claim {
            task,
            ttl,
            request_id: id,
        } => {
            put("space", json!(space));
            put("task_id", json!(task));
            if let Some(ttl) = ttl {
                put("ttl_secs", json!(ttl));
            }
            ("work_claim", request_id(id)?)
        }
        RemoteOp::ClaimPath {
            project,
            path,
            task,
            ttl,
            request_id: id,
        } => {
            put("space", json!(space));
            put("project", json!(project));
            put("path", json!(path));
            if let Some(task) = task {
                put("task_id", json!(task));
            }
            if let Some(ttl) = ttl {
                put("ttl_secs", json!(ttl));
            }
            ("work_claim_path", request_id(id)?)
        }
        RemoteOp::Renew { claim, fence, ttl } => {
            put("space", json!(space));
            put("claim_id", json!(claim));
            put("fence", json!(fence));
            if let Some(ttl) = ttl {
                put("ttl_secs", json!(ttl));
            }
            ("work_renew", Retry::QueryFirst("verify CLAIM FENCE"))
        }
        RemoteOp::Release { claim, fence, note } => {
            put("space", json!(space));
            put("claim_id", json!(claim));
            put("fence", json!(fence));
            if let Some(note) = note {
                put("note", json!(note));
            }
            ("work_release", Retry::QueryFirst("verify CLAIM FENCE"))
        }
        RemoteOp::State {
            claim,
            fence,
            state,
            note,
            request_id: id,
        } => {
            put("space", json!(space));
            put("claim_id", json!(claim));
            put("fence", json!(fence));
            put("state", json!(state));
            if let Some(note) = note {
                put("note", json!(note));
            }
            ("work_set_state", request_id(id)?)
        }
        RemoteOp::Handoff {
            claim,
            fence,
            to_agent,
            to_machine,
            ttl,
            note,
            request_id: id,
        } => {
            put("space", json!(space));
            put("claim_id", json!(claim));
            put("fence", json!(fence));
            put("to_agent", json!(to_agent));
            put("to_machine", json!(to_machine));
            if let Some(ttl) = ttl {
                put("ttl_secs", json!(ttl));
            }
            if let Some(note) = note {
                put("note", json!(note));
            }
            ("work_handoff", request_id(id)?)
        }
        RemoteOp::SharePull { after_seq, limit } => {
            put("space", json!(space));
            put("after_seq", json!(after_seq));
            put("limit", json!(limit));
            ("share_pull", Retry::Read)
        }
        RemoteOp::ShareQuery { limit } => {
            put("space", json!(space));
            put("limit", json!(limit));
            ("share_query", Retry::Read)
        }
        RemoteOp::Verify { claim, fence } => {
            put("space", json!(space));
            put("claim_id", json!(claim));
            put("fence", json!(fence));
            ("work_verify", Retry::Read)
        }
    };
    Ok((tool, args, retry))
}
