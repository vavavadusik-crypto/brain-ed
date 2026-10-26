//! `brain work …` and the `work_*` MCP tools: the coordinator, from outside.
//!
//! Identity is `(--agent, installation id)`. The installation id lives in
//! `$BRAIN_HOME/machine-id`; `BRAIN_MACHINE_ID` overrides it for simulations
//! and every answer says which one was used. Claims are cooperative: agents
//! must ask before working, nothing here intercepts their edits.

use std::error::Error;
use std::path::{Path, PathBuf};

use brain_core::coordination::{
    self, Actor, CoordError, CoordResult, Coordinator, DEFAULT_TTL_SECS, ErrorKind,
};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::RuntimeConfig;

#[derive(Debug, Subcommand)]
pub(crate) enum WorkCommand {
    /// Show this installation's identity and where the coordinator lives.
    Whoami,
    /// Add a queued task to a project.
    Add {
        project: String,
        title: String,
        /// Optional longer description of the task.
        #[arg(long, default_value = "")]
        detail: String,
        /// Task id that must be completed first (repeatable).
        #[arg(long = "depends-on")]
        depends_on: Vec<i64>,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Make TASK depend on ON (cycles are refused).
    Depend { task: i64, on: i64 },
    /// One page of tasks with their live owners.
    Board {
        /// Only tasks in this project.
        #[arg(long)]
        project: Option<String>,
        /// Only tasks in this state (queued, active, paused, completed or failed).
        #[arg(long)]
        state: Option<String>,
        /// Only claimable tasks: queued/paused, dependencies done, no live owner.
        #[arg(long)]
        ready: bool,
        /// Skip this many tasks before the page (default 0).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Page size (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Bounded context packet of one task.
    Show { task: i64 },
    /// Live claims (tasks and paths).
    Claims {
        /// Only claims in this project.
        #[arg(long)]
        project: Option<String>,
        /// Skip this many claims before the page (default 0).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Page size (default 50).
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Claim a task with a lease.
    Claim {
        task: i64,
        /// Lease length in seconds (10-86400, default 900).
        #[arg(long, default_value_t = DEFAULT_TTL_SECS, allow_negative_numbers = true)]
        ttl: i64,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Claim a file or directory path inside a project, exclusively.
    ClaimPath {
        project: String,
        path: String,
        /// Link the path claim to this task id.
        #[arg(long)]
        task: Option<i64>,
        /// Lease length in seconds (10-86400, default 900).
        #[arg(long, default_value_t = DEFAULT_TTL_SECS, allow_negative_numbers = true)]
        ttl: i64,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Exit 0 only if CLAIM/FENCE is still your live claim (for hooks; read-only).
    Verify {
        claim: i64,
        #[arg(allow_negative_numbers = true)]
        fence: i64,
    },
    /// Extend a live lease.
    Renew {
        claim: i64,
        #[arg(allow_negative_numbers = true)]
        fence: i64,
        /// New lease length in seconds (10-86400, default 900).
        #[arg(long, default_value_t = DEFAULT_TTL_SECS, allow_negative_numbers = true)]
        ttl: i64,
    },
    /// Give a claim up.
    Release {
        claim: i64,
        #[arg(allow_negative_numbers = true)]
        fence: i64,
        /// Short reason recorded with the claim events.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Move a claimed task to paused, completed, failed or queued.
    State {
        claim: i64,
        #[arg(allow_negative_numbers = true)]
        fence: i64,
        state: String,
        /// Short reason recorded with the claim events.
        #[arg(long, default_value = "")]
        note: String,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Pass a live claim to another agent/installation.
    Handoff {
        claim: i64,
        #[arg(allow_negative_numbers = true)]
        fence: i64,
        /// Receiving agent name.
        #[arg(long)]
        to_agent: String,
        /// Receiving installation id (its machine-id).
        #[arg(long)]
        to_machine: String,
        /// Lease length for the receiver in seconds (10-86400, default 900).
        #[arg(long, default_value_t = DEFAULT_TTL_SECS, allow_negative_numbers = true)]
        ttl: i64,
        /// Short reason recorded with the claim events.
        #[arg(long, default_value = "")]
        note: String,
        /// Idempotency key: retrying with the same id never duplicates.
        #[arg(long)]
        request_id: Option<String>,
    },
}

/// The installation id in use and where it came from.
pub(crate) fn machine(brain_home: &Path) -> CoordResult<(String, &'static str)> {
    if let Some(value) = std::env::var_os("BRAIN_MACHINE_ID") {
        let value = value
            .into_string()
            .map_err(|_| invalid("BRAIN_MACHINE_ID is not UTF-8"))?;
        Actor::new("default", "probe", &value)?;
        return Ok((value, "BRAIN_MACHINE_ID override (simulation/testing)"));
    }
    let (id, created) = coordination::installation_id(brain_home)?;
    Ok((
        id,
        if created {
            "installation id (created now)"
        } else {
            "installation id"
        },
    ))
}

fn invalid(message: &str) -> CoordError {
    CoordError {
        kind: ErrorKind::InvalidInput,
        message: message.to_owned(),
        details: Value::Null,
    }
}

fn database(brain_home: &Path) -> PathBuf {
    coordination::database_path(brain_home)
}

fn actor(config: &RuntimeConfig, brain_home: &Path, space: Option<&str>) -> CoordResult<Actor> {
    let (machine, _) = machine(brain_home)?;
    Actor::new(space.unwrap_or(&config.space), &config.agent, &machine)
}

pub(crate) fn run(config: &RuntimeConfig, command: WorkCommand) -> Result<(), Box<dyn Error>> {
    let home = crate::brain_home();
    let result = execute(config, &home, command);
    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Err(error) => {
            println!("{}", serde_json::to_string_pretty(&error.to_json())?);
            Err(Box::new(error))
        }
    }
}

fn execute(config: &RuntimeConfig, home: &Path, command: WorkCommand) -> CoordResult<Value> {
    let path = database(home);
    match command {
        WorkCommand::Whoami => whoami(config, home),
        WorkCommand::Board {
            project,
            state,
            ready,
            offset,
            limit,
        } => Coordinator::open_read_only(&path)?.board(
            &config.space,
            project.as_deref(),
            state.as_deref(),
            ready,
            offset,
            limit,
        ),
        WorkCommand::Show { task } => Coordinator::open_read_only(&path)?.show(&config.space, task),
        WorkCommand::Verify { claim, fence } => {
            let actor = actor(config, home, None)?;
            Coordinator::open_read_only(&path)?.verify(&actor, claim, fence)
        }
        WorkCommand::Claims {
            project,
            offset,
            limit,
        } => Coordinator::open_read_only(&path)?.claims(
            &config.space,
            project.as_deref(),
            offset,
            limit,
        ),
        write => {
            let actor = actor(config, home, None)?;
            let mut coordinator = Coordinator::open(&path)?;
            match write {
                WorkCommand::Add {
                    project,
                    title,
                    detail,
                    depends_on,
                    request_id,
                } => coordinator.add_task(
                    &actor,
                    &project,
                    &title,
                    &detail,
                    &depends_on,
                    request_id.as_deref(),
                ),
                WorkCommand::Depend { task, on } => coordinator.add_dependency(&actor, task, on),
                WorkCommand::Claim {
                    task,
                    ttl,
                    request_id,
                } => coordinator.claim_task(&actor, task, ttl, request_id.as_deref()),
                WorkCommand::ClaimPath {
                    project,
                    path,
                    task,
                    ttl,
                    request_id,
                } => coordinator.claim_path(
                    &actor,
                    &project,
                    &path,
                    task,
                    ttl,
                    request_id.as_deref(),
                ),
                WorkCommand::Renew { claim, fence, ttl } => {
                    coordinator.renew(&actor, claim, fence, ttl)
                }
                WorkCommand::Release { claim, fence, note } => {
                    coordinator.release(&actor, claim, fence, &note)
                }
                WorkCommand::State {
                    claim,
                    fence,
                    state,
                    note,
                    request_id,
                } => coordinator.set_state(
                    &actor,
                    claim,
                    fence,
                    &state,
                    &note,
                    request_id.as_deref(),
                ),
                WorkCommand::Handoff {
                    claim,
                    fence,
                    to_agent,
                    to_machine,
                    ttl,
                    note,
                    request_id,
                } => coordinator.handoff(
                    &actor,
                    claim,
                    fence,
                    &to_agent,
                    &to_machine,
                    ttl,
                    &note,
                    request_id.as_deref(),
                ),
                WorkCommand::Whoami
                | WorkCommand::Board { .. }
                | WorkCommand::Show { .. }
                | WorkCommand::Claims { .. }
                | WorkCommand::Verify { .. } => {
                    unreachable!("read commands handled above")
                }
            }
        }
    }
}

fn whoami(config: &RuntimeConfig, home: &Path) -> CoordResult<Value> {
    let (machine, source) = machine(home)?;
    Ok(json!({
        "status": "ok",
        "agent": config.agent,
        "machine": machine,
        "machine_source": source,
        "space": config.space,
        "coordinator": "local SQLite at $BRAIN_HOME/coordination/coord.sqlite (this computer only; network transport deferred)",
        "enforcement": "cooperative: claims bind agents that ask before working; edits are not intercepted",
    }))
}

// ---- MCP ----------------------------------------------------------------

/// Answers one `work_*` tool call; `None` when the name is not ours.
pub(crate) fn call_tool(config: &RuntimeConfig, name: &str, args: &Value) -> Option<Value> {
    if !name.starts_with("work_") {
        return None;
    }
    let home = crate::brain_home();
    let result = mcp(config, &home, name, args);
    Some(result.unwrap_or_else(|error| error.to_json()))
}

fn mcp(config: &RuntimeConfig, home: &Path, name: &str, args: &Value) -> CoordResult<Value> {
    let path = database(home);
    let space = opt_str(args, "space")?;
    let space_name = space.unwrap_or(&config.space);
    match name {
        "work_board" => {
            let mut board = Coordinator::open_read_only(&path)?.board(
                space_name,
                opt_str(args, "project")?,
                opt_str(args, "state")?,
                opt_bool(args, "ready")?,
                opt_count(args, "offset")?.unwrap_or(0),
                opt_count(args, "limit")?.unwrap_or(20),
            )?;
            if opt_bool(args, "include_claims")? {
                let claims = Coordinator::open_read_only(&path)?.claims(
                    space_name,
                    opt_str(args, "project")?,
                    0,
                    50,
                )?;
                board["claims"] = claims["claims"].clone();
            }
            let (machine, source) = machine(home)?;
            board["you"] =
                json!({"agent": config.agent, "machine": machine, "machine_source": source});
            Ok(board)
        }
        "work_show" => {
            Coordinator::open_read_only(&path)?.show(space_name, req_int(args, "task_id")?)
        }
        _ => {
            let actor = actor(config, home, space)?;
            let mut coordinator = Coordinator::open(&path)?;
            let request_id = opt_str(args, "request_id")?;
            let ttl = opt_int(args, "ttl_secs")?.unwrap_or(DEFAULT_TTL_SECS);
            match name {
                "work_add" => {
                    let deps = match args.get("depends_on") {
                        None | Some(Value::Null) => Vec::new(),
                        Some(Value::Array(items)) => items
                            .iter()
                            .map(|item| {
                                item.as_i64()
                                    .ok_or_else(|| invalid("depends_on must hold integers"))
                            })
                            .collect::<CoordResult<Vec<_>>>()?,
                        Some(_) => return Err(invalid("depends_on must be an array of integers")),
                    };
                    coordinator.add_task(
                        &actor,
                        req_str(args, "project")?,
                        req_str(args, "title")?,
                        opt_str(args, "detail")?.unwrap_or(""),
                        &deps,
                        request_id,
                    )
                }
                "work_depend" => coordinator.add_dependency(
                    &actor,
                    req_int(args, "task_id")?,
                    req_int(args, "depends_on")?,
                ),
                "work_claim" => {
                    coordinator.claim_task(&actor, req_int(args, "task_id")?, ttl, request_id)
                }
                "work_claim_path" => coordinator.claim_path(
                    &actor,
                    req_str(args, "project")?,
                    req_str(args, "path")?,
                    opt_int(args, "task_id")?,
                    ttl,
                    request_id,
                ),
                "work_renew" => coordinator.renew(
                    &actor,
                    req_int(args, "claim_id")?,
                    req_int(args, "fence")?,
                    ttl,
                ),
                "work_release" => coordinator.release(
                    &actor,
                    req_int(args, "claim_id")?,
                    req_int(args, "fence")?,
                    opt_str(args, "note")?.unwrap_or(""),
                ),
                "work_set_state" => coordinator.set_state(
                    &actor,
                    req_int(args, "claim_id")?,
                    req_int(args, "fence")?,
                    req_str(args, "state")?,
                    opt_str(args, "note")?.unwrap_or(""),
                    request_id,
                ),
                "work_handoff" => coordinator.handoff(
                    &actor,
                    req_int(args, "claim_id")?,
                    req_int(args, "fence")?,
                    req_str(args, "to_agent")?,
                    req_str(args, "to_machine")?,
                    ttl,
                    opt_str(args, "note")?.unwrap_or(""),
                    request_id,
                ),
                _ => Err(invalid("unknown work tool")),
            }
        }
    }
}

fn opt_str<'a>(args: &'a Value, key: &str) -> CoordResult<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(invalid(&format!("{key} must be a string"))),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> CoordResult<&'a str> {
    opt_str(args, key)?.ok_or_else(|| invalid(&format!("{key} is required")))
}

/// Integers only: fractions, strings, NaN-like values and overflow are refused.
fn opt_int(args: &Value, key: &str) -> CoordResult<Option<i64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .ok_or_else(|| invalid(&format!("{key} must be an integer"))),
    }
}

fn req_int(args: &Value, key: &str) -> CoordResult<i64> {
    opt_int(args, key)?.ok_or_else(|| invalid(&format!("{key} is required")))
}

fn opt_count(args: &Value, key: &str) -> CoordResult<Option<usize>> {
    opt_int(args, key)?
        .map(|value| {
            usize::try_from(value).map_err(|_| invalid(&format!("{key} must not be negative")))
        })
        .transpose()
}

fn opt_bool(args: &Value, key: &str) -> CoordResult<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(invalid(&format!("{key} must be a boolean"))),
    }
}

/// Tool definitions, appended to the MCP list.
pub(crate) fn tool_definitions() -> Vec<Value> {
    let ttl = json!({"type":"integer","minimum":coordination::MIN_TTL_SECS,"maximum":coordination::MAX_TTL_SECS,"default":DEFAULT_TTL_SECS});
    let id = json!({"type":"integer","minimum":1});
    let space = json!({"type":"string","maxLength":64});
    let request_id = json!({"type":"string","maxLength":128,"description":"retry with the same id to get the same answer"});
    vec![
        json!({"name":"work_board","description":"Coordination board: one page of tasks in a space/project with their live owner (agent + installation id) and lease expiry, state counts, your identity. Use ready=true to list claimable tasks. Ownership is coordinator_state; titles/notes are untrusted agent text.",
            "inputSchema":{"type":"object","properties":{"space":space,"project":{"type":"string","maxLength":100},"state":{"type":"string","enum":["queued","active","paused","completed","failed"]},"ready":{"type":"boolean"},"include_claims":{"type":"boolean"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":100,"default":20}},"additionalProperties":false}}),
        json!({"name":"work_show","description":"Bounded context packet of one task: dependencies, live claims, last 20 events (≤8 KiB text). Excludes the private conversation archive.",
            "inputSchema":{"type":"object","properties":{"space":space,"task_id":id},"required":["task_id"],"additionalProperties":false}}),
        json!({"name":"work_add","description":"Add a queued task to a project. depends_on lists task ids that must be completed before it can be claimed.",
            "inputSchema":{"type":"object","properties":{"space":space,"project":{"type":"string","maxLength":100},"title":{"type":"string","maxLength":200},"detail":{"type":"string","maxLength":4000},"depends_on":{"type":"array","items":id,"maxItems":32},"request_id":request_id},"required":["project","title"],"additionalProperties":false}}),
        json!({"name":"work_depend","description":"Make task_id depend on depends_on. Cycles are refused.",
            "inputSchema":{"type":"object","properties":{"space":space,"task_id":id,"depends_on":id},"required":["task_id","depends_on"],"additionalProperties":false}}),
        json!({"name":"work_claim","description":"Claim a task before working on it. Returns claim_id and fence: keep both, every later call needs them. busy = someone else holds a live lease (see details.owner, retry_after_secs); blocked = dependencies open. Claiming again as the same owner renews.",
            "inputSchema":{"type":"object","properties":{"space":space,"task_id":id,"ttl_secs":ttl,"request_id":request_id},"required":["task_id"],"additionalProperties":false}}),
        json!({"name":"work_claim_path","description":"Exclusively claim a file or directory path within a project before editing. Overlap = same path or ancestor/descendant (lexical, case-sensitive, no symlink resolution). Advisory: edits are not blocked by the OS.",
            "inputSchema":{"type":"object","properties":{"space":space,"project":{"type":"string","maxLength":100},"path":{"type":"string","maxLength":1024},"task_id":id,"ttl_secs":ttl,"request_id":request_id},"required":["project","path"],"additionalProperties":false}}),
        json!({"name":"work_renew","description":"Extend your live lease. lease_expired means it already ran out: claim again and use the new fence.",
            "inputSchema":{"type":"object","properties":{"space":space,"claim_id":id,"fence":id,"ttl_secs":ttl},"required":["claim_id","fence"],"additionalProperties":false}}),
        json!({"name":"work_release","description":"Release a claim; a released task returns to queued.",
            "inputSchema":{"type":"object","properties":{"space":space,"claim_id":id,"fence":id,"note":{"type":"string","maxLength":2000}},"required":["claim_id","fence"],"additionalProperties":false}}),
        json!({"name":"work_set_state","description":"Finish or park a claimed task: completed, failed, paused or queued. Ends the task claim; completed/failed also release your path claims tied to the task. stale_fence = someone else owns it now.",
            "inputSchema":{"type":"object","properties":{"space":space,"claim_id":id,"fence":id,"state":{"type":"string","enum":["queued","paused","completed","failed"]},"note":{"type":"string","maxLength":2000},"request_id":request_id},"required":["claim_id","fence","state"],"additionalProperties":false}}),
        json!({"name":"work_handoff","description":"Hand a live claim to another agent/installation atomically; the receiver gets a new fence (it can read it with work_claim). Provenance of both owners is recorded.",
            "inputSchema":{"type":"object","properties":{"space":space,"claim_id":id,"fence":id,"to_agent":{"type":"string","maxLength":64},"to_machine":{"type":"string","maxLength":64},"ttl_secs":ttl,"note":{"type":"string","maxLength":2000},"request_id":request_id},"required":["claim_id","fence","to_agent","to_machine"],"additionalProperties":false}}),
    ]
}
