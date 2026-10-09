//! `brain net share …`: opt-in shared memory between installations.
//!
//! Host-local administration (`enable`, `disable`, `status`, `grant`,
//! `ungrant`, `publish`, `retract`, `supersede`, `facts`) touches only the
//! explicit `--state-dir` and never the network. Grants live in the auth
//! store and are written here only — no request payload, MCP argument or
//! claimed provenance can create one. `publish` curates exactly one fact
//! from explicit arguments; nothing reads the private memory DB or archive.
//!
//! `pull` is the only networked command: it reads one peer's own event
//! stream over `brain-coord/1` TLS (`share_pull`), checks every event's
//! digest and the pinned `--origin`, and applies pages atomically with the
//! local cursor. A retraction reaches this installation only by pulling.

use std::path::Path;

use brain_core::network::{AuthStore, SHARE_STORE_FILE, TlsCallResult};
use brain_core::sharing::{
    CursorState, FactKind, FactRow, MAX_PAGE, MAX_WIRE_PAGE, ShareError, ShareEvent, ShareStore,
    SharedFact, TrustedPeer,
};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use super::{
    ConnectArgs, Exit, Retry, StateDirArg, client, existing_store, failure, net_error,
    outcome_document, transport, unix_now,
};

/// Upper bound on pages one `pull` run applies (10 events each).
const MAX_PULL_PAGES: u32 = 100;

#[derive(Debug, Subcommand)]
pub(crate) enum ShareCommand {
    /// Host-local: allow a space to be served and received. Default is off.
    Enable(SpaceArgs),
    /// Host-local: stop serving and receiving a space (already exported copies remain with peers).
    Disable(SpaceArgs),
    /// Host-local: origin id, enabled spaces, counts and memory grants (no payloads, no secrets).
    Status(StateDirArg),
    /// Host-local: let one enrolled principal read one shared space (must be inside its namespace).
    Grant(GrantArgs),
    /// Host-local: withdraw a memory grant; effective on the next request.
    Ungrant(GrantArgs),
    /// Host-local: publish ONE curated fact under this installation's origin.
    Publish(PublishArgs),
    /// Host-local: retract one of this installation's live facts (peers see it when they pull).
    Retract(FactRef),
    /// Host-local: replace one live fact's payload; history stays in the event stream.
    Supersede(PublishArgs),
    /// Host-local: list facts stored here (own and pulled), with provenance.
    Facts(FactsArgs),
    /// Pull one peer's own shared events over TLS and apply them atomically.
    Pull(PullArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SpaceArgs {
    #[command(flatten)]
    state: StateDirArg,
    #[arg(long)]
    space: String,
}

#[derive(Debug, Args)]
pub(crate) struct GrantArgs {
    #[command(flatten)]
    state: StateDirArg,
    #[arg(long)]
    machine: String,
    #[arg(long)]
    agent_id: String,
    #[arg(long)]
    space: String,
}

#[derive(Debug, Args)]
pub(crate) struct FactRef {
    #[command(flatten)]
    state: StateDirArg,
    #[arg(long)]
    space: String,
    #[arg(long)]
    fact_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct PublishArgs {
    #[command(flatten)]
    fact: FactRef,
    /// fact or link.
    #[arg(long, default_value = "fact", value_parser = ["fact", "link"])]
    kind: String,
    /// The curated payload: one JSON object, at most 4096 canonical bytes.
    #[arg(long)]
    payload: String,
    /// Optional human-readable claim, at most 600 bytes.
    #[arg(long)]
    claim: Option<String>,
    /// Recorded as claimed provenance only; never authority.
    #[arg(long)]
    claimed_agent: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct FactsArgs {
    #[command(flatten)]
    state: StateDirArg,
    #[arg(long)]
    space: String,
    #[arg(long)]
    include_retracted: bool,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=MAX_PAGE as i64))]
    limit: u32,
}

#[derive(Debug, Args)]
pub(crate) struct PullArgs {
    /// Local state directory whose share store receives the events.
    #[arg(long, value_name = "DIR")]
    state_dir: std::path::PathBuf,
    #[arg(long)]
    space: String,
    /// Local label of the peer we pull from (trusted host context, recorded with each fact).
    #[arg(long)]
    peer_id: String,
    /// The origin id the peer must serve (from its `share status`); anything else is refused.
    #[arg(long)]
    origin: String,
    #[command(flatten)]
    connect: ConnectArgs,
}

pub(crate) fn run(command: ShareCommand) -> (Exit, Value) {
    match command {
        ShareCommand::Enable(args) => with_store(&args.state.state_dir, |store| {
            store.enable_space(&args.space)?;
            Ok(json!({"status": "ok", "space": args.space, "enabled": true}))
        }),
        ShareCommand::Disable(args) => with_store(&args.state.state_dir, |store| {
            let changed = store.disable_space(&args.space)?;
            Ok(
                json!({"status": "ok", "space": args.space, "enabled": false, "changed": changed,
                "note": "copies already pulled by peers are not erased"}),
            )
        }),
        ShareCommand::Status(args) => status(&args.state_dir),
        ShareCommand::Grant(args) => grant(&args, true),
        ShareCommand::Ungrant(args) => grant(&args, false),
        ShareCommand::Publish(args) => with_store(&args.fact.state.state_dir, |store| {
            let fact = curated(&args)?;
            let seq = store.publish(&args.fact.space, fact, args.claimed_agent.as_deref())?;
            Ok(
                json!({"status": "ok", "origin": store.install_id(), "space": args.fact.space,
                "fact_id": args.fact.fact_id, "seq": seq, "exported": "only when a granted peer pulls"}),
            )
        }),
        ShareCommand::Supersede(args) => with_store(&args.fact.state.state_dir, |store| {
            let fact = curated(&args)?;
            let seq = store.supersede(
                &args.fact.space,
                &args.fact.fact_id,
                fact,
                args.claimed_agent.as_deref(),
            )?;
            Ok(
                json!({"status": "ok", "origin": store.install_id(), "space": args.fact.space,
                "fact_id": args.fact.fact_id, "seq": seq}),
            )
        }),
        ShareCommand::Retract(args) => with_store(&args.state.state_dir, |store| {
            let seq = store.retract(&args.space, &args.fact_id)?;
            Ok(
                json!({"status": "ok", "origin": store.install_id(), "space": args.space,
                "fact_id": args.fact_id, "seq": seq, "propagates": "when peers pull"}),
            )
        }),
        ShareCommand::Facts(args) => with_store(&args.state.state_dir, |store| {
            let facts = store.local_facts(&args.space, args.include_retracted, args.limit)?;
            Ok(
                json!({"status": "ok", "origin": store.install_id(), "space": args.space,
                "facts": facts.iter().map(|fact| fact_json(store, fact)).collect::<Vec<_>>()}),
            )
        }),
        ShareCommand::Pull(args) => pull(&args),
    }
}

fn share_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(SHARE_STORE_FILE)
}

fn share_failure(error: &ShareError) -> (Exit, Value) {
    let exit = match error {
        ShareError::Storage(_) | ShareError::Incompatible { .. } => Exit::InvalidConfig,
        _ => Exit::DomainError,
    };
    (
        exit,
        failure("share_error", error.kind(), &error.to_string()),
    )
}

fn with_store(
    state_dir: &Path,
    action: impl FnOnce(&mut ShareStore) -> Result<Value, ShareError>,
) -> (Exit, Value) {
    let mut store = match ShareStore::open(share_path(state_dir)) {
        Ok(store) => store,
        Err(error) => return share_failure(&error),
    };
    match action(&mut store) {
        Ok(document) => (Exit::Ok, document),
        Err(error) => share_failure(&error),
    }
}

fn curated(args: &PublishArgs) -> Result<SharedFact, ShareError> {
    let payload: Value = serde_json::from_str(&args.payload)
        .map_err(|error| ShareError::InvalidData(format!("--payload is not JSON: {error}")))?;
    Ok(SharedFact {
        fact_id: args.fact.fact_id.clone(),
        kind: FactKind::parse(&args.kind)?,
        space: args.fact.space.clone(),
        payload,
        claim: args.claim.clone(),
    })
}

fn fact_json(store: &ShareStore, fact: &FactRow) -> Value {
    json!({
        "origin": fact.origin,
        "local": fact.origin == store.install_id(),
        "fact_id": fact.fact_id,
        "kind": fact.kind.as_str(),
        "status": fact.status.as_str(),
        "payload": fact.payload,
        "claim": fact.claim,
        "origin_seq": fact.origin_seq,
        "last_seq": fact.last_seq,
        "claimed_agent": fact.claimed_agent,
        "trusted_peer": fact.peer_id,
        "applied_at": fact.applied_at,
    })
}

fn status(state_dir: &Path) -> (Exit, Value) {
    with_store(state_dir, |store| {
        let enabled = store.enabled_spaces()?;
        let mut spaces = Vec::new();
        for space in &enabled {
            let counts = store.counts(space)?;
            spaces.push(json!({"space": counts.space, "enabled": counts.enabled, "own_events": counts.own_events,
                "own_live_facts": counts.own_live_facts, "received_live_facts": counts.received_live_facts}));
        }
        let grants = if state_dir.join("auth.sqlite").is_file() {
            AuthStore::open(state_dir)
                .and_then(|auth| auth.list_memory_grants(200))
                .map_or_else(
                    |error| json!({"error": net_error(&error)}),
                    |list| json!(list),
                )
        } else {
            json!([])
        };
        Ok(json!({
            "status": "ok",
            "origin": store.install_id(),
            "enabled_spaces": spaces,
            "memory_grants": grants,
            "served_over": "brain net serve (loopback TLS) as share_pull/share_query; nothing is pushed",
        }))
    })
}

fn grant(args: &GrantArgs, allow: bool) -> (Exit, Value) {
    let store = match existing_store(&args.state.state_dir) {
        Ok(store) => store,
        Err(failed) => return failed,
    };
    let target = json!({"machine": args.machine, "agent": args.agent_id, "space": args.space});
    let outcome = if allow {
        store
            .grant_memory_read(&args.machine, &args.agent_id, &args.space)
            .map(|new| json!({"granted": target, "new": new}))
    } else {
        store
            .revoke_memory_read(&args.machine, &args.agent_id, &args.space)
            .map(|()| json!({"ungranted": target}))
    };
    match outcome {
        Ok(mut document) => {
            document["status"] = json!("ok");
            document["effective"] = json!("next request; the host must also enable the space");
            (Exit::Ok, document)
        }
        Err(error) => {
            let exit = if error.status() == 400 {
                Exit::InvalidConfig
            } else {
                Exit::Refused
            };
            (
                exit,
                json!({"status": "refused", "target": target, "error": net_error(&error)}),
            )
        }
    }
}

fn pull(args: &PullArgs) -> (Exit, Value) {
    let mut store = match ShareStore::open(share_path(&args.state_dir)) {
        Ok(store) => store,
        Err(error) => return share_failure(&error),
    };
    let tls = match client(&args.connect) {
        Ok(client) => client,
        Err(failed) => return failed,
    };
    let endpoint = transport(&tls, &args.connect);
    let mut cursor = match store.cursor(&args.peer_id, &args.space) {
        Ok(cursor) => cursor.get(&args.origin).unwrap_or(0),
        Err(error) => return share_failure(&error),
    };
    let start = cursor;
    let mut applied_pages = 0_u32;
    let mut received = 0_usize;
    loop {
        let mut arguments = Map::new();
        arguments.insert("space".into(), json!(args.space));
        arguments.insert("after_seq".into(), json!(cursor));
        arguments.insert("limit".into(), json!(MAX_WIRE_PAGE));
        let body = match tls.call("share_pull", "", arguments, None) {
            TlsCallResult::Ok(body) => body,
            other => {
                let (exit, mut document) = outcome_document(other, &Retry::Read);
                document["endpoint"] = endpoint;
                document["cursor"] = json!(cursor);
                return (exit, document);
            }
        };
        let page = match parse_page(&body, &args.origin, &args.space) {
            Ok(page) => page,
            Err(error) => {
                let (exit, mut document) = share_failure(&error);
                document["cursor"] = json!(cursor);
                return (exit, document);
            }
        };
        let peer = TrustedPeer::new(&args.peer_id, format!("unix:{}", unix_now()));
        let expected = CursorState::with(&args.origin, cursor);
        match store.apply_batch(&args.space, &peer, &expected, &page.0) {
            Ok(next) => cursor = next.get(&args.origin).unwrap_or(cursor),
            Err(error) => {
                let (exit, mut document) = share_failure(&error);
                document["cursor"] = json!(cursor);
                document["atomic"] =
                    json!("the failed page was rolled back; the cursor did not move");
                return (exit, document);
            }
        }
        received += page.0.len();
        applied_pages += 1;
        if !page.1 || page.0.is_empty() {
            break;
        }
        if applied_pages >= MAX_PULL_PAGES {
            return (
                Exit::Ok,
                json!({"status": "ok", "complete": false, "space": args.space, "origin": args.origin,
                "from_seq": start, "cursor": cursor, "events": received, "endpoint": endpoint,
                "note": "page budget reached; run pull again"}),
            );
        }
    }
    (
        Exit::Ok,
        json!({"status": "ok", "complete": true, "space": args.space, "origin": args.origin, "peer_id": args.peer_id,
            "from_seq": start, "cursor": cursor, "events": received, "pages": applied_pages, "endpoint": endpoint}),
    )
}

/// Validates one `share_pull` response: the pinned origin and space, a
/// strictly parsed event list (digest checked per event) and `has_more`.
fn parse_page(
    body: &Value,
    origin: &str,
    space: &str,
) -> Result<(Vec<ShareEvent>, bool), ShareError> {
    let bad =
        |what: &str| ShareError::InvalidData(format!("malformed share_pull response: {what}"));
    let share = body
        .pointer("/result/share")
        .ok_or_else(|| bad("missing result.share"))?;
    if share.get("origin").and_then(Value::as_str) != Some(origin) {
        return Err(ShareError::InvalidData(
            "origin_mismatch: the peer serves a different origin than --origin".into(),
        ));
    }
    if share.get("space").and_then(Value::as_str) != Some(space) {
        return Err(bad("space"));
    }
    let has_more = share
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(|| bad("has_more"))?;
    let list = share
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("events"))?;
    if list.len() > MAX_WIRE_PAGE as usize {
        return Err(bad("more events than one page allows"));
    }
    let mut events = Vec::with_capacity(list.len());
    for raw in list {
        let event = ShareEvent::from_wire(raw)?;
        if event.origin != origin || event.space != space {
            return Err(ShareError::InvalidData(
                "origin_mismatch: an event names another origin or space".into(),
            ));
        }
        events.push(event);
    }
    Ok((events, has_more))
}
