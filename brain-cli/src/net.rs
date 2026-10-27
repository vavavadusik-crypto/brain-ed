//! `brain net …`: explicit TLS coordination and an optional company relay.
//!
//! * `serve` defaults to loopback; LAN/CGNAT listeners require opt-in. With
//!   `--via-relay`, loopback TLS also accepts outgoing registered tunnels.
//! * `relay serve` is an explicitly public listener with separate name/role
//!   tokens. It unwraps only outer relay TLS; coordinator TLS stays end to end.
//! * Local administration and join use explicit state paths. Client identity
//!   comes from the coordinator credential, never the global `--agent`.
//! * Clients pin the coordinator CA and SAN/Host independently of the route.
//!   Only a failed direct TCP connect may select a configured relay. Requests
//!   are never replayed after a TLS refusal or a possible dispatch.
//! * Secrets are read from owner-only files/descriptors, never argv or output.
//!   Serving commands print readiness JSON and stop when stdin closes.

use std::error::Error;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::bundle;
use brain_core::network::auth::{DEFAULT_CREDENTIAL_TTL_SECS, MIN_CREDENTIAL_TTL_SECS};
use brain_core::network::tls::{MAX_DEADLINE, MIN_DEADLINE};
use brain_core::network::{
    AuthStore, KeySource, NetError, TcpDialer, TlsCallResult, TlsClientConfig, TlsCoordClient,
    TlsCoordServer, TlsError, TlsServerConfig, allowed_tools, parse_dial_target_with,
    tools_for_role, write_credential_file,
};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use crate::RuntimeConfig;

mod relay;
mod share;
pub(crate) mod team;

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
    /// Company-hosted NAT relay with separate scoped tokens and end-to-end coordinator TLS.
    Relay {
        #[command(subcommand)]
        command: relay::RelayCommand,
    },
    /// Trusted local admin: create/refresh a principal and write a new credential file (0600).
    ///
    /// The token goes to --credential-out only; stdout shows its token_id.
    Enroll(EnrollArgs),
    /// Trusted local admin: revoke one credential, or disable a machine and all its credentials.
    Revoke(RevokeArgs),
    /// Trusted local admin: list principals (no secrets, no credential hashes).
    Principals(StateDirArg),
    /// Create the coordinator's own CA and server certificate (rcgen, EC P-256).
    Init(InitArgs),
    /// Unpack a join bundle: CA, credential and endpoint for this machine.
    Join(JoinArgs),
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
pub(crate) struct InitArgs {
    #[command(flatten)]
    state: StateDirArg,
    /// DNS name for the server certificate SAN (repeatable).
    #[arg(long = "name", value_name = "NAME")]
    names: Vec<String>,
    /// Numeric IP for the SAN (repeatable).
    #[arg(long = "ip", value_name = "IP")]
    ips: Vec<String>,
    /// Replace existing certificates; without this a repeat init changes nothing.
    #[arg(long)]
    rotate: bool,
}

#[derive(Debug, Args)]
pub(crate) struct JoinArgs {
    /// The bundle file written by `net enroll --bundle-out`.
    bundle: PathBuf,
    /// Client directory (default: $BRAIN_HOME/net/client).
    #[arg(long, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// Allow dialing private LAN addresses; stored in endpoint.json.
    #[arg(long)]
    allow_lan_dial: bool,
    /// Allow dialing shared CGNAT/overlay addresses; stored in endpoint.json.
    #[arg(long)]
    allow_cgnat_dial: bool,
    /// Delete the bundle after a successful unpack.
    #[arg(long)]
    consume: bool,
    /// Replace an already joined endpoint (the old one is kept as `<dir>.previous`).
    #[arg(long)]
    replace: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ServeArgs {
    #[command(flatten)]
    relay: relay::RelayArgs,
    #[command(flatten)]
    state: StateDirArg,
    /// Server certificate chain (PEM, owned by this uid); default DIR/tls/server.pem.
    #[arg(long, value_name = "FILE")]
    cert: Option<PathBuf>,
    /// Server private key (PEM, 0600, no symlink).
    #[arg(long, value_name = "FILE", conflicts_with = "key_fd")]
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
    /// Opt in to listening on a shared CGNAT/overlay address (RFC 6598 range, e.g. Tailscale IPv4).
    #[arg(long)]
    allow_cgnat: bool,
    /// Absolute per-connection deadline in milliseconds (50..=5000).
    #[arg(long, default_value_t = 5000)]
    deadline_ms: u64,
}

#[derive(Debug, Args)]
pub(crate) struct EnrollArgs {
    #[command(flatten)]
    relay: relay::RelayArgs,
    #[command(flatten)]
    state: StateDirArg,
    /// Machine (installation) id, [a-z0-9-]{3,64}.
    #[arg(long)]
    machine: String,
    /// Agent id bound to the credential, [A-Za-z0-9._:@-]{1,64}.
    #[arg(long)]
    agent_id: String,
    /// worker (read and write), manager (read only), or legacy reader.
    #[arg(long, value_parser = ["worker", "manager", "reader"])]
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
    /// Also write a versioned join bundle (CA + credential + endpoints) 0600.
    #[arg(long, value_name = "FILE")]
    bundle_out: Option<PathBuf>,
    /// Dial endpoints for the bundle (repeatable, IP:PORT).
    #[arg(long = "dial", value_name = "IP:PORT")]
    bundle_dial: Vec<String>,
    /// Server name for the bundle; default: first DNS name of the server cert.
    #[arg(long, value_name = "NAME")]
    server_name: Option<String>,
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

#[derive(Clone, Debug, Args, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConnectArgs {
    #[command(flatten)]
    #[serde(default)]
    relay: relay::RelayArgs,
    /// Numeric dial target, e.g. 127.0.0.1:7443; default: endpoint.json from `net join`.
    #[arg(long, value_name = "IP:PORT")]
    dial: Option<String>,
    /// Opt in to dialing a private unicast LAN address (same policy as --allow-lan).
    #[arg(long)]
    allow_lan_dial: bool,
    /// Opt in to dialing a shared CGNAT/overlay address (same policy as --allow-cgnat).
    #[arg(long)]
    allow_cgnat_dial: bool,
    /// Name the server certificate must carry (SAN); default: endpoint.json.
    #[arg(long, value_name = "NAME")]
    server_name: Option<String>,
    /// The only CA trusted for this call (PEM); default: endpoint.json. System roots are never used.
    #[arg(long, value_name = "FILE")]
    ca: Option<PathBuf>,
    /// Credential file (0600, no symlink, owned by this uid); default: endpoint.json.
    #[arg(long, value_name = "FILE", conflicts_with = "credential_fd")]
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
        /// Only tasks in this project.
        #[arg(long)]
        project: Option<String>,
        /// Only tasks in this state (queued, active, paused, completed or failed).
        #[arg(long, value_parser = ["queued", "active", "paused", "completed", "failed"])]
        state: Option<String>,
        /// Only claimable tasks: queued/paused, dependencies done, no live owner.
        #[arg(long)]
        ready: bool,
        /// Skip this many tasks before the page (default 0).
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Page size (1-200, default 50).
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
        /// Only claims in this project.
        #[arg(long)]
        project: Option<String>,
        /// Skip this many claims before the page (default 0).
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Page size (1-200, default 50).
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: u32,
    },
    /// Add a queued task. Safe to retry with the same --request-id.
    Add {
        project: String,
        title: String,
        /// Optional longer description of the task.
        #[arg(long)]
        detail: Option<String>,
        /// Task id that must be completed first (repeatable).
        #[arg(long = "depends-on", value_parser = positive())]
        depends_on: Vec<i64>,
        /// Idempotency key: retrying with the same id never duplicates.
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
        /// Lease length in seconds (10-86400; default: the coordinator's).
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Claim a path. Safe to retry with the same --request-id.
    ClaimPath {
        project: String,
        path: String,
        /// Link the path claim to this task id.
        #[arg(long, value_parser = positive())]
        task: Option<i64>,
        /// Lease length in seconds (10-86400; default: the coordinator's).
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Extend a lease. No request id: on an unknown outcome, query `verify` first.
    Renew {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        /// New lease length in seconds (10-86400; default: the coordinator's).
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
    },
    /// Give a claim up. No request id: on an unknown outcome, query `verify` first.
    Release {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        /// Short reason recorded with the claim events.
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
        /// Short reason recorded with the claim events.
        #[arg(long)]
        note: Option<String>,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Pass a live claim on. Safe to retry with the same --request-id.
    Handoff {
        #[arg(value_parser = positive())]
        claim: i64,
        #[arg(value_parser = positive())]
        fence: i64,
        /// Receiving agent name.
        #[arg(long)]
        to_agent: String,
        /// Receiving installation id.
        #[arg(long)]
        to_machine: String,
        /// Lease length for the receiver in seconds (10-86400; default: the coordinator's).
        #[arg(long, value_parser = positive())]
        ttl: Option<i64>,
        /// Short reason recorded with the claim events.
        #[arg(long)]
        note: Option<String>,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Read one page of the host's own shared events in --space (needs a host memory grant).
    SharePull {
        /// Read only events after this shared-log sequence number (default 0).
        #[arg(long, default_value_t = 0)]
        after_seq: u64,
        /// Page size (1-10, default 10).
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=10))]
        limit: u32,
    },
    /// Live facts the host itself published in --space (needs a host memory grant).
    ShareQuery {
        /// Page size (1-10, default 10).
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
        NetCommand::Init(args) => {
            match bundle::init(&args.state.state_dir, &args.names, &args.ips, args.rotate) {
                Ok(document) => (Exit::Ok, document),
                Err(error) => invalid(error.to_string()),
            }
        }
        NetCommand::Join(args) => {
            let client_dir = args
                .state_dir
                .clone()
                .unwrap_or_else(|| crate::bundle::default_client_dir(&crate::brain_home()));
            match bundle::join(
                &args.bundle,
                &client_dir,
                args.allow_lan_dial,
                args.allow_cgnat_dial,
                args.consume,
                args.replace,
            ) {
                Ok(document) => (Exit::Ok, document),
                Err(error) => invalid(error.to_string()),
            }
        }
        NetCommand::Relay { command } => return relay::run(command),
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
/// Classifies the predictable serve failures before the real start attempt:
/// the bind probe yields `address_in_use` / `address_not_available` /
/// `permission_denied`; unreadable certificate or key material yields
/// `tls_material`; anything else keeps the previous category (`unavailable`).
/// No paths or secrets ever appear in the category.
fn serve_error_category(
    listen: std::net::IpAddr,
    port: u16,
    allow_lan: bool,
    allow_cgnat: bool,
    cert: &Path,
    key: Option<&Path>,
) -> &'static str {
    // The listen-address policy first: a policy refusal must keep the exact
    // previous error text that start() produces (existing tests pin it), so
    // this helper reports the previous category and lets start() speak.
    if brain_core::network::server::checked_listen_ip_with(listen, allow_lan, allow_cgnat).is_err()
    {
        return "unavailable";
    }
    // Material second: unreadable certificate or key files yield the most
    // specific category. Only real paths can be probed; a descriptor cannot
    // be read without consuming it.
    if let Some(key) = key
        && (std::fs::read(cert).is_err() || std::fs::read(key).is_err())
    {
        return "tls_material";
    }
    let probe = std::net::TcpListener::bind(std::net::SocketAddr::new(listen, port));
    match probe {
        Err(error) => match error.kind() {
            std::io::ErrorKind::AddrInUse => "address_in_use",
            std::io::ErrorKind::AddrNotAvailable => "address_not_available",
            std::io::ErrorKind::PermissionDenied => "permission_denied",
            _ => "unavailable",
        },
        Ok(listener) => {
            drop(listener);
            "unavailable"
        }
    }
}

fn net_error(error: &NetError) -> Value {
    json!({"http_status": error.status(), "detail": error.body()})
}

// ---- serve ---------------------------------------------------------------

fn serve(args: &ServeArgs) -> Result<(), Box<dyn Error>> {
    let relay_route = args.relay.route()?;
    if relay_route.is_some()
        && !args
            .listen
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Err("--via-relay coordinator must listen on loopback".into());
    }
    let relay_connector = relay_route
        .as_ref()
        .map(brain_core::network::relay::Connector::new)
        .transpose()?;
    let deadline = match deadline(args.deadline_ms) {
        Ok(deadline) => deadline,
        Err((exit, document)) => return finish(exit, &document),
    };
    // Default certificate material: DIR/tls written by `net init`.
    let cert = args
        .cert
        .clone()
        .unwrap_or_else(|| args.state.state_dir.join("tls/server.pem"));
    // Understandable serve-failure categories (E7.0c): classified BEFORE
    // TlsCoordServer::start from things this process can observe without
    // leaking paths or secrets. The bind probe has a tiny race window; when
    // start still fails on the bind, the previous category is kept.
    // The material probe uses the effective paths: an explicit --key or the
    // default DIR/tls/server.key. A descriptor key cannot be probed without
    // consuming it, so --key-fd skips the key side of the check.
    let default_key = args.state.state_dir.join("tls/server.key");
    let key_probe: Option<&Path> = if args.key_fd.is_some() {
        None
    } else {
        Some(args.key.as_deref().unwrap_or(&default_key))
    };
    let category = serve_error_category(
        args.listen
            .parse()
            .unwrap_or(std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST)),
        args.port,
        args.allow_lan,
        args.allow_cgnat,
        &cert,
        key_probe,
    );
    // A classified failure is self-contained: category only, no paths, no
    // secrets, and no start attempt (the material/port state will not change
    // inside this process).
    if category != "unavailable" {
        let http_status = if category == "tls_material" { 400 } else { 503 };
        return finish(
            if category == "tls_material" {
                Exit::InvalidConfig
            } else {
                Exit::Unreachable
            },
            &json!({
                "status": "not_serving",
                "error": {
                    "detail": {"error": category, "message": category},
                    "http_status": http_status,
                },
                "category": category,
            }),
        );
    }
    let key = match (&args.key, args.key_fd) {
        (Some(path), None) => KeySource::File(path.clone()),
        (None, Some(fd)) => KeySource::Fd(fd),
        (None, None) => KeySource::File(args.state.state_dir.join("tls/server.key")),
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
    let mut config = TlsServerConfig::new(&args.state.state_dir, args.port, &cert, PathBuf::new());
    config.key = key;
    config.deadline = deadline;
    config.listen = listen;
    config.allow_lan = args.allow_lan;
    config.allow_cgnat = args.allow_cgnat;
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
                &json!({"status": "not_serving", "error": net_error(&error), "category": category}),
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
    let relay_handle = relay_connector
        .map(|connector| {
            brain_core::network::relay::register(
                connector,
                std::net::SocketAddr::new(listen, handle.port()),
            )
        })
        .transpose()?;
    // Local observation for the settings window, including non-default state dirs
    // and ephemeral/LAN ports. No credentials enter this private document.
    let observation_path = window_observation_path(&crate::brain_home());
    let observation = json!({"pid": std::process::id(),
        "started": process_start(std::process::id()),
        "bind": std::net::SocketAddr::new(listen, handle.port()).to_string(),
        "state_dir": std::fs::canonicalize(&args.state.state_dir)?});
    crate::private_fs::ensure_private_home(observation_path.parent().ok_or("missing Brain home")?)?;
    let runtime_metadata =
        std::fs::symlink_metadata(observation_path.parent().ok_or("runtime directory")?)?;
    if !runtime_metadata.is_dir()
        || Some(runtime_metadata.uid()) != crate::private_fs::current_uid()
        || runtime_metadata.mode() & 0o077 != 0
    {
        handle.stop();
        return Err("unsafe coordinator runtime directory".into());
    }
    if let Err(error) = crate::private_fs::write_private(
        &observation_path,
        serde_json::to_vec(&observation)?.as_slice(),
    ) {
        handle.stop();
        return Err(error.into());
    }
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
    drop(relay_handle);
    let joined = handle.stop();
    if std::fs::read(&observation_path).ok().as_deref()
        == Some(serde_json::to_vec(&observation)?.as_slice())
    {
        let _ = std::fs::remove_file(&observation_path);
    }
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
    let relay_bundle = match args.relay.bundle() {
        Ok(route) => route,
        Err(_) => return invalid("invalid relay route"),
    };
    if relay_bundle.is_some() && args.bundle_out.is_none() {
        return invalid("--via-relay on enroll requires --bundle-out");
    }
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
    let mut document = json!({
        "status": "ok",
        "principal": {"machine": args.machine, "agent": args.agent_id, "role": args.role, "spaces": args.grant_space, "label": args.label},
        "token_id": token_id,
        "expires_at": issued_at + ttl,
        "credential_file": out,
        "secret": "written to credential_file (0600) only",
        "capabilities": tools_for_role(&args.role),
    });
    if let Some(bundle_out) = &args.bundle_out {
        let server_name = args.server_name.clone().unwrap_or_else(|| {
            bundle::first_dns_name(&args.state.state_dir).unwrap_or_else(|| "localhost".into())
        });
        match bundle::write_bundle(
            &args.state.state_dir,
            &server_name,
            &args.bundle_dial,
            out,
            &args.machine,
            &args.agent_id,
            issued_at + ttl,
            bundle_out,
            relay_bundle,
        ) {
            Ok(written) => {
                document["bundle"] = written["bundle"].clone();
                document["bundle_server_name"] = json!(server_name);
            }
            Err(error) => {
                return (
                    Exit::InvalidConfig,
                    json!({"status": "invalid_config", "error": error.to_string(), "note": "the credential file was written; the bundle was not"}),
                );
            }
        }
    }
    (Exit::Ok, document)
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

/// Private runtime observation names are scoped to the local Brain home.
fn window_observation_path(brain_home: &Path) -> PathBuf {
    // Per-user, per-Brain-home runtime metadata; net serve must not create a
    // private memory store on coordinator-only installations.
    let uid = crate::private_fs::current_uid().unwrap_or(u32::MAX);
    let key = crate::installer::sha256(brain_home.as_os_str().as_encoded_bytes());
    std::env::temp_dir()
        .join(format!("brain-ed-net-{uid}"))
        .join(format!("{key}.json"))
}

#[cfg(target_os = "macos")]
fn process_start(_pid: u32) -> Option<String> {
    // No verified process-generation identity: stale observations are not trusted.
    None
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<String> {
    // /proc stat field 22; comm can contain spaces and parentheses.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)
        .map(str::to_owned)
}

fn window_observation(brain_home: &Path) -> Option<Value> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(window_observation_path(brain_home))
        .ok()
        .filter(|file| {
            file.metadata().ok().is_some_and(|meta| {
                meta.is_file()
                    && Some(meta.uid()) == crate::private_fs::current_uid()
                    && meta.mode() & 0o077 == 0
            })
        })
        .and_then(|file| serde_json::from_reader::<_, Value>(file.take(16 * 1024)).ok())
}

pub(crate) fn window_state_dir(brain_home: &Path) -> PathBuf {
    window_observation(brain_home)
        .and_then(|v| v["state_dir"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| brain_home.join("net"))
}

pub(crate) fn window_state_document(brain_home: &Path) -> Value {
    let observation = window_observation(brain_home);
    let observed_dir = observation
        .as_ref()
        .and_then(|value| value["state_dir"].as_str());
    let state_dir = observed_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| brain_home.join("net"));
    let principals = AuthStore::principal_count_read_only(&state_dir).ok();
    let bind = observation
        .as_ref()
        .and_then(|value| value["bind"].as_str());
    #[cfg(target_os = "linux")]
    let serving = observation.as_ref().and_then(|value| {
        let pid = u32::try_from(value["pid"].as_u64()?).ok()?;
        Some(
            process_start(pid)
                .as_deref()
                .is_some_and(|started| Some(started) == value["started"].as_str()),
        )
    });
    #[cfg(target_os = "macos")]
    let serving: Option<bool> = None;
    json!({
        "team": team::manifest_document(&state_dir).unwrap_or_else(|_|json!({"error":"team_manifest_unavailable"})),
        "auth_store": state_dir.join("auth.sqlite").is_file(),
        "principals": principals,
        "coordination_database": state_dir.join("coord.sqlite").is_file(),
        "serving": serving,
        "bind": bind,
        "note": "Local process observation; unknown if no coordinator started by this version for this Brain home. No authenticated remote health check.",
    })
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

/// Connection parameters after merging the explicit flags with the endpoint
/// `net join` wrote. Explicit flags always win; the endpoint fills the rest.
struct ResolvedConnect {
    relay: Option<brain_core::network::relay::Route>,
    dial: Option<String>,
    server_name: String,
    ca: PathBuf,
    credential: Option<PathBuf>,
    credential_fd: Option<i32>,
    allow_lan_dial: bool,
    allow_cgnat_dial: bool,
    deadline_ms: u64,
    from_endpoint: bool,
}

fn resolve_connect(
    connect: &ConnectArgs,
    use_endpoint: bool,
) -> Result<ResolvedConnect, (Exit, Value)> {
    let endpoint = use_endpoint
        .then(|| bundle::load_endpoint(&bundle::default_client_dir(&crate::brain_home())))
        .flatten();
    let endpoint_value = endpoint.unwrap_or(Value::Null);
    let endpoint_str = |key: &str| {
        endpoint_value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    // The endpoint stores the dial list (the bundle's `--dial` repeats); the
    // first entry is the primary target.
    let dial = connect.dial.clone().or_else(|| {
        endpoint_value
            .get("dial")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| endpoint_str("dial"))
    });
    let relay = if connect.relay.via_relay.is_some() {
        connect
            .relay
            .route()
            .map_err(|_| invalid("invalid relay route"))?
    } else {
        endpoint_value
            .get("relay")
            .filter(|v| !v.is_null())
            .map(|v| {
                let mut route: brain_core::network::relay::Route =
                    serde_json::from_value(v.clone())
                        .map_err(|_| invalid("invalid relay endpoint"))?;
                let dir = bundle::default_client_dir(&crate::brain_home());
                route.ca = dir.join(route.ca);
                route.token_file = dir.join(route.token_file);
                Ok::<_, (Exit, Value)>(route)
            })
            .transpose()?
    };
    if dial.is_none() && relay.is_none() {
        return Err(invalid(
            "--dial is required (no endpoint.json: run brain net join or pass the flags)",
        ));
    }
    let server_name = connect
        .server_name
        .clone()
        .or_else(|| endpoint_str("server_name"))
        .unwrap_or_else(|| "localhost".into());
    let default_ca =
        endpoint_str("ca").map(|name| bundle::default_client_dir(&crate::brain_home()).join(name));
    let ca = match connect.ca.clone().or(default_ca) {
        Some(ca) => ca,
        None => {
            return Err(invalid(
                "--ca is required (no endpoint.json: run brain net join or pass the flags)",
            ));
        }
    };
    let default_credential = endpoint_str("credential")
        .map(|name| bundle::default_client_dir(&crate::brain_home()).join(name));
    let credential = connect.credential.clone().or(default_credential);
    // Address policy from the endpoint is a floor, never silently widened:
    // a flag can only add permission, not remove it.
    let allow_lan_dial = connect.allow_lan_dial
        || endpoint_value
            .get("allow_lan_dial")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let allow_cgnat_dial = connect.allow_cgnat_dial
        || endpoint_value
            .get("allow_cgnat_dial")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    Ok(ResolvedConnect {
        relay,
        dial,
        server_name,
        ca,
        credential,
        credential_fd: connect.credential_fd,
        allow_lan_dial,
        allow_cgnat_dial,
        deadline_ms: connect.deadline_ms,
        from_endpoint: !endpoint_value.is_null(),
    })
}

fn client(connect: &ConnectArgs) -> Result<(TlsCoordClient, bool), (Exit, Value)> {
    client_with_endpoint(connect, true)
}
// A saved team peer supplies its own complete trust and routing configuration;
// an unrelated local net-join endpoint must never fill in its relay route.
fn client_explicit(connect: &ConnectArgs) -> Result<(TlsCoordClient, bool), (Exit, Value)> {
    client_with_endpoint(connect, false)
}
fn client_with_endpoint(
    connect: &ConnectArgs,
    use_endpoint: bool,
) -> Result<(TlsCoordClient, bool), (Exit, Value)> {
    let resolved = resolve_connect(connect, use_endpoint)?;
    let deadline = deadline(resolved.deadline_ms)?;
    let dial = parse_dial_target_with(
        resolved.dial.as_deref().unwrap_or("127.0.0.1:1"),
        resolved.allow_lan_dial,
        resolved.allow_cgnat_dial,
    )
    .map_err(|error| invalid(tls_message(&error)))?;
    let credential = match (&resolved.credential, resolved.credential_fd) {
        (Some(path), None) => KeySource::File(path.clone()),
        (None, Some(fd)) => KeySource::Fd(fd),
        _ => return Err(invalid("exactly one of --credential / --credential-fd")),
    };
    let mut config = TlsClientConfig::new(&resolved.ca, &resolved.server_name, dial);
    config.deadline = deadline;
    config.allow_lan = resolved.allow_lan_dial;
    config.allow_cgnat = resolved.allow_cgnat_dial;
    let dialer: Arc<dyn brain_core::network::Dialer> = match &resolved.relay {
        Some(route) => Arc::new(brain_core::network::relay::RelayDialer {
            direct: resolved.dial.is_some(),
            connector: brain_core::network::relay::Connector::new(route)
                .map_err(|_| invalid("invalid relay configuration"))?,
        }),
        None => Arc::new(TcpDialer),
    };
    let built = TlsCoordClient::with_dialer(config, &credential, dialer).map_err(|error| {
        (
            Exit::InvalidConfig,
            json!({"status": "invalid_config", "error": net_error(&error)}),
        )
    })?;
    Ok((built, resolved.from_endpoint))
}

fn tls_message(error: &TlsError) -> String {
    error.to_string()
}

fn transport(client: &TlsCoordClient, connect: &ConnectArgs) -> Value {
    let resolved = resolve_connect(connect, true).ok();
    json!({
        "protocol": PROTOCOL,
        "tls": true,
        "dial": resolved.as_ref().and_then(|r| r.dial.as_ref()),
        "relay": resolved.as_ref().and_then(|r| r.relay.as_ref()).map(|r| json!({"address":r.address,"name":r.name,"server_name":r.server_name})),
        "selection": "direct TCP first when configured; relay only on TCP connect failure",
        "server_name": client.server_name(),
        "pinned_ca": resolved.as_ref().map(|r| &r.ca),
        "system_roots": false,
    })
}

fn status(connect: &ConnectArgs) -> (Exit, Value) {
    let (client, from_endpoint) = match client(connect) {
        Ok(found) => found,
        Err(failed) => return failed,
    };
    let endpoint = transport(&client, connect);
    let mut endpoint = endpoint;
    endpoint["from_endpoint_json"] = json!(from_endpoint);
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
        return invalid(
            "tool is not allowlisted (the typed CLI can only name coordinator tools; update brain on both sides if you expect a newer tool)",
        );
    }
    let (client, _from_endpoint) = match client(connect) {
        Ok(found) => found,
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

#[cfg(test)]
mod panel_tests {
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_process_generation_is_explicitly_unknown() {
        assert!(super::process_start(std::process::id()).is_none());
    }

    use super::*;
    #[test]
    fn coordinator_observation_tracks_actual_process_and_custom_state_without_writes() {
        let home = crate::test_support::temp_path("net-panel");
        let state = home.join("explicit-coordinator");
        let store = AuthStore::open(&state).unwrap();
        let coordinator =
            brain_core::coordination::Coordinator::open(&state.join("coord.sqlite")).unwrap();
        let path = window_observation_path(&home);
        crate::private_fs::ensure_private_home(path.parent().unwrap()).unwrap();
        let observation = json!({"pid": std::process::id(), "started": process_start(std::process::id()),
            "bind":"127.0.0.1:32145", "state_dir":state});
        crate::private_fs::write_private(&path, &serde_json::to_vec(&observation).unwrap())
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let doc = window_state_document(&home);
        #[cfg(target_os = "linux")]
        assert_eq!(doc["serving"], true);
        #[cfg(target_os = "macos")]
        assert_eq!(doc["serving"], Value::Null);
        assert_eq!(doc["bind"], "127.0.0.1:32145");
        assert_eq!(doc["principals"], 0);
        assert_eq!(doc["coordination_database"], true);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let stale = json!({"pid":std::process::id(), "started":"wrong-start", "bind":"127.0.0.1:32145", "state_dir":state});
        crate::private_fs::write_private(&path, &serde_json::to_vec(&stale).unwrap()).unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(window_state_document(&home)["serving"], false);
        #[cfg(target_os = "macos")]
        assert_eq!(window_state_document(&home)["serving"], Value::Null);
        std::fs::remove_file(path).unwrap();
        assert_eq!(window_state_document(&home)["serving"], Value::Null);
        drop(store);
        drop(coordinator);
        let _ = std::fs::remove_dir_all(home);
    }
}
