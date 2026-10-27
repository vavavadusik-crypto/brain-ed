#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod scheduler;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io;
#[cfg(feature = "model2vec")]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(feature = "model2vec")]
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::embeddings::{DEFAULT_ENGLISH_MODEL2VEC_MODEL, DEFAULT_MODEL2VEC_MODEL, Embedder};
#[cfg(feature = "model2vec")]
use brain_core::embeddings::{Model2VecEmbedder, install_model, model_is_cached};
use brain_core::{
    Agent, AsOf, Attribution, HybridSearch, MemoryKind, MemoryRecord, MemoryService,
    RememberRequest, SearchLayers, SearchQuery, Session, Space, SpaceKind, SqliteStorage, Storage,
    Timestamp,
};
use brain_core::{NewNode, NodeKind};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};

use archive_rank::{ArchiveRanking, fuse_conversations};

mod activity;
mod archive;
mod archive_rank;
mod background;
mod backup;
mod brief;
mod bundle;
mod client_home;
mod client_integration;
mod client_registry;
mod desktop;
mod distill;
mod distill_schedule;
mod doctor;
mod embedding_runtime;
mod explore;
mod graph_view;
mod hook;
mod import;
mod installer;
mod llm;
mod llm_command;
mod maintenance;

mod map;
mod mcp;
mod net;
mod otel_export;
mod private_fs;
mod redact;
mod reindex;
mod remote_mcp;
mod settings;
mod settings_history;
mod stats;
#[cfg(test)]
mod test_support;
mod ui;
mod when_phrase;
mod work;

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_MEMORY_SCAN: usize = 1_000_000;

/// Local, durable memory for AI agents.
#[derive(Debug, Parser)]
#[command(name = "brain", version, about)]
struct Cli {
    /// SQLite database path (default: ~/.brain/brain.sqlite).
    #[arg(long, global = true, value_name = "PATH")]
    database: Option<PathBuf>,

    /// Memory namespace.
    #[arg(long, global = true, default_value = "default")]
    space: String,

    /// Agent identity used for attribution and memory ownership.
    #[arg(long, global = true, default_value_t = default_agent())]
    agent: String,

    /// Model cache directory (default: ~/.brain/models).
    #[arg(long, global = true, value_name = "DIR")]
    models_dir: Option<PathBuf>,

    /// Host-only: the `brain net` state directory whose shared facts map
    /// packets may read (absolute path). Unset, no shared fact is read; no
    /// MCP argument can set or change it.
    #[arg(long, global = true, value_name = "DIR")]
    share_state_dir: Option<PathBuf>,

    /// Forbid even explicit model installation; ordinary commands never auto-download.
    #[arg(long, global = true, env = "BRAIN_OFFLINE")]
    offline: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create or migrate a local Brain database.
    Init {
        /// Optional positional database path, kept for `brain init PATH`.
        #[arg(value_name = "DATABASE")]
        path: Option<PathBuf>,

        /// Detect Claude, Codex, and Cursor user configs and connect Brain.
        #[arg(long)]
        agents: bool,

        /// Show every proposed agent-config diff without writing anything.
        #[arg(long, requires = "agents")]
        dry_run: bool,

        /// Apply non-conflicting agent-config changes without interactive prompts.
        #[arg(long, requires = "agents")]
        yes: bool,
    },

    /// Store an attributed, bi-temporal memory record.
    Remember(RememberArgs),

    /// Search code, memory, and the private conversation archive.
    Ask(SearchArgs),

    /// Search only records written through the memory command.
    Recall(SearchArgs),

    /// Index a source tree into the code graph.
    Index {
        /// Directory to index.
        #[arg(value_name = "PATH")]
        path: PathBuf,

        /// Reindex every file, ignoring content hashes.
        #[arg(long)]
        force: bool,
    },

    /// Show when a topic was worked on.
    When {
        /// Topic to look for.
        topic: String,
    },

    /// Show what happened in a period.
    Timeline {
        /// Month as YYYY-MM.
        #[arg(long)]
        month: Option<String>,
    },

    /// Explain why a decision was taken.
    Explain {
        /// Decision to explain.
        decision: String,
    },

    /// List what is currently in the way.
    Blockers {
        /// Most blockers to return.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Summarise where the work stands.
    Checkpoint,

    /// Show what the work is aiming at.
    Goals,

    /// State the goal outright, rather than leaving it to be inferred.
    Goal {
        #[command(subcommand)]
        action: GoalAction,
    },

    /// Show or change quality settings.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Inspect settings history or restore only search profile/overrides.
    Settings {
        #[command(subcommand)]
        action: SettingsAction,
    },

    /// Report database counts, salience layers, size, and freshness.
    Status,

    /// Consistent snapshots of the brain with rotation and restore.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },

    /// One-off repairs over the stored database.
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },

    /// Plug-in LLM providers (compute nodes).
    Llm {
        #[command(subcommand)]
        command: LlmCommand,
    },

    /// Distill durable decisions and errors from archived sessions.
    Distill {
        #[command(subcommand)]
        command: DistillCommand,
    },

    /// What the brain holds per source (Graphify, Markdown folders, agent
    /// memory, conversation archive) and how fresh each source is.
    Stats,

    /// Graphify: search the code/knowledge graph (runs `graphify query`).
    Query {
        /// Words to look for.
        question: String,
        /// Extra arguments passed to graphify unchanged.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// Graphify: shortest path between two graph nodes (runs `graphify path`).
    Path {
        from: String,
        to: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },

    /// Graphify graph commands that share a name with a Brain command.
    Graph {
        #[command(subcommand)]
        command: GraphCommand,
    },

    /// Inspect integration and capture coverage without writing or starting services.
    Doctor,

    /// List, inspect, forget, or export memory owned by this agent.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },

    /// Run the MCP server over newline-delimited stdio JSON-RPC.
    #[command(visible_alias = "mcp")]
    Serve {
        /// Opt-in: an owner-only JSON file naming remote coordinators (absolute path).
        /// Adds five read-only remote_* tools; without it the tool list is unchanged.
        /// Remote tool calls use TLS even with --offline (model downloads only).
        #[arg(long, value_name = "FILE")]
        remote_config: Option<PathBuf>,
    },

    /// Read the private conversation archive, or switch and run its collector.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommand,
    },

    /// Coordinate work between agents and installations: tasks, claims, leases.
    Work {
        #[command(subcommand)]
        command: work::WorkCommand,
    },

    /// The work map: links, timeline and context packets built from local records.
    Map {
        #[command(subcommand)]
        command: map::MapCommand,
    },

    /// Publish and query opt-in, typed facts for a team.
    Team {
        #[command(subcommand)]
        command: net::team::TeamCommand,
    },

    /// The coordinator over TLS: serve, enroll, revoke, call.
    Net {
        #[command(subcommand)]
        command: net::NetCommand,
    },

    /// Accept one agent hook event from JSON stdin and enqueue its safe metadata.
    Hook(hook::HookArgs),

    /// Build the cached project context brief (at most 600 tokens).
    Brief(BriefArgs),

    /// Manage explicitly approved local embedding models.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },

    /// Find past work resembling a task you are about to start.
    Similar {
        /// What you are about to do.
        query: String,
    },

    /// Assemble a template from how similar work actually went.
    Template {
        /// What you are about to do.
        query: String,
    },

    /// Open the settings window on a local-only web page.
    Ui(ui::UiArgs),

    /// Rebuild embeddings, optionally under a different model.
    Reindex(reindex::ReindexArgs),

    /// Refresh the imported part of the graph from a Graphify export.
    Import(import::ImportArgs),

    /// Explicitly registered agent-client homes (registration only: no client
    /// integration, discovery or capture).
    Clients {
        #[command(subcommand)]
        action: client_registry::ClientsCommand,
    },

    /// Remove every agent integration recorded by `brain init --agents`.
    Uninstall {
        /// Show exact restoration diffs without writing anything.
        #[arg(long)]
        dry_run: bool,

        /// Restore all non-drifted files without interactive prompts.
        #[arg(long)]
        yes: bool,
    },

    /// Internal: process pending background jobs (embedding queue).
    #[command(name = "__worker", hide = true)]
    Worker,

    /// Internal: download one embedding model into the models directory.
    #[command(name = "__model-download", hide = true)]
    ModelDownload {
        /// Model identifier to download (see `brain model list`).
        #[arg(long)]
        model_id: String,
    },
}

#[derive(Debug, Args)]
struct RememberArgs {
    /// Text to preserve.
    text: String,

    /// Memory kind: fact, decision, task, gotcha, error, or note.
    #[arg(long, default_value = "fact")]
    kind: String,

    /// Retrieval tag; may be supplied more than once.
    #[arg(long = "tag")]
    tags: Vec<String>,

    /// Source path or URL.
    #[arg(long)]
    source: Option<String>,

    /// Source confidence/reliability in the range zero through one.
    #[arg(long, default_value_t = 0.8)]
    source_weight: f64,

    /// World-time validity start as Unix seconds (default: now).
    #[arg(long)]
    valid_from: Option<Timestamp>,

    /// Optional exclusive world-time validity end as Unix seconds.
    #[arg(long)]
    valid_to: Option<Timestamp>,
}

#[derive(Clone, Debug, Args)]
struct SearchArgs {
    /// Natural-language query.
    query: String,

    /// Hard upper bound for persisted conservative token estimates.
    #[arg(long)]
    budget_tokens: Option<usize>,

    /// Skip the private conversation archive for this call.
    #[arg(long)]
    no_archive: bool,

    /// Restrict archive hits to this role; repeatable. One of user, assistant,
    /// tool_call, tool_result, tool_error, reasoning. Default: every role.
    #[arg(long, value_name = "ROLE")]
    archive_role: Vec<String>,

    /// Quality profile: fast, balanced, precise, or max.
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,

    /// Include warm and cold salience layers in addition to hot.
    #[arg(long)]
    all_layers: bool,

    /// Load the embedding model even when the words already answered.
    ///
    /// Only meaningful in the "on demand" mode, which otherwise skips the
    /// model — and the 2.5 seconds it costs — for a query the lexical search
    /// answered on its own.
    #[arg(long)]
    semantic: bool,
}

#[derive(Clone, Debug, Args)]
struct BriefArgs {
    /// Project working directory (default: current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Git branch override when the directory is not locally inspectable.
    #[arg(long)]
    git_branch: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ModelChoice {
    Multilingual,
    English,
}

impl ModelChoice {
    const fn model_id(self) -> &'static str {
        match self {
            Self::Multilingual => DEFAULT_MODEL2VEC_MODEL,
            Self::English => DEFAULT_ENGLISH_MODEL2VEC_MODEL,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// Ask permission, then download with progress, timeout, and resume support.
    Install {
        /// Model language profile.
        #[arg(long, value_enum, default_value_t = ModelChoice::Multilingual)]
        model: ModelChoice,

        /// Kill a stalled downloader after this many seconds.
        #[arg(long, default_value_t = 900)]
        timeout_seconds: u64,

        /// Approve the advertised download non-interactively.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ArchiveCommand {
    /// What is configured, what the collector observed, and what is stored.
    Status,

    /// Full-text search with bounded excerpts; results are untrusted text.
    Search {
        /// Words to find; operators and quotes are matched literally.
        query: String,

        /// Only this client: claude, codex, or hermes.
        #[arg(long)]
        client: Option<String>,

        /// Only this role: user, assistant, tool_call, tool_result, tool_error
        /// or reasoning.
        #[arg(long)]
        role: Option<String>,

        /// Only this session id.
        #[arg(long)]
        session: Option<String>,

        /// Only messages whose recorded working directory is this path or below it.
        #[arg(long)]
        project_root: Option<String>,

        /// Most hits to return (1-20).
        #[arg(long)]
        limit: Option<usize>,

        /// Upper bound on the estimated tokens of the answer (500-4000).
        #[arg(long)]
        budget_tokens: Option<usize>,
    },

    /// Export the journal as OpenTelemetry GenAI JSON (no network).
    Export {
        /// Only this format is supported; the flag exists for future formats.
        #[arg(long, value_parser = ["otlp-json"], default_value = "otlp-json")]
        format: String,

        /// Only this session id.
        #[arg(long)]
        session: Option<String>,

        /// Only entries said at or after this UTC time (ISO-8601 or Unix seconds).
        #[arg(long)]
        since: Option<String>,

        /// Only entries said before this UTC time (ISO-8601 or Unix seconds).
        #[arg(long)]
        until: Option<String>,

        /// Export metadata without the (already masked) text content.
        #[arg(long)]
        no_content: bool,
    },

    /// Read one entry, a bounded slice at a time, with its provenance.
    Read {
        /// Entry id from a search result.
        id: String,

        /// Byte offset into the entry text, from a previous `next_offset`.
        #[arg(long)]
        offset: Option<usize>,

        /// Most text bytes to return (256-32768).
        #[arg(long)]
        max_bytes: Option<usize>,

        /// Neighbouring entries of the same session to preview (0-5 each side).
        #[arg(long)]
        context: Option<usize>,
    },

    /// Run the bounded collector once (what the timer runs).
    Collect {
        /// Print one summary line instead of the full JSON document.
        #[arg(long)]
        quiet: bool,
    },

    /// Let the collector run.
    Enable {
        /// Collect sources modified within this many days, plus every source already followed.
        #[arg(long)]
        since_days: Option<u64>,
        /// Comma-separated clients to read (claude, codex, hermes). Omitted keeps the
        /// current selection; a config that never had one reads all three.
        #[arg(long, value_delimiter = ',')]
        clients: Option<Vec<String>>,
        /// Comma-separated roles to collect. Omitted keeps all roles.
        #[arg(long, value_delimiter = ',')]
        roles: Option<Vec<String>>,
    },

    /// Pause the collector; runs exit without reading any source.
    Disable,

    /// Install or remove the user timer (systemd/launchd) that runs `collect` every 2 minutes.
    Timer {
        #[arg(value_parser = ["install", "remove"])]
        action: String,
    },

    /// Explicitly admit older, out-of-window history a few files at a time.
    Backfill {
        #[command(subcommand)]
        action: BackfillCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BackfillCommand {
    /// Show the backfill switch and the honest coverage of what is stored.
    Status,

    /// Start or restart backfill, admitting up to N older files per run (1-20).
    Start {
        /// How many older files per run may newly enter the archive (1-20).
        #[arg(long)]
        files_per_run: u64,
    },

    /// Stop admitting new older files; already-tracked sources keep being caught up.
    Stop,
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// List active memory records owned by this agent.
    List {
        /// Maximum number of records.
        #[arg(long, default_value_t = DEFAULT_LIST_LIMIT)]
        limit: usize,
    },

    /// Show one active memory record owned by this agent.
    Show {
        /// Stable memory ID.
        id: String,
    },

    /// Forget one memory without erasing its bi-temporal history.
    Delete {
        /// Stable memory ID.
        id: String,
    },

    /// Export active memory owned by this agent as JSON.
    Export {
        /// Write JSON to this file instead of standard output.
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
}

#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) database: PathBuf,
    pub(crate) models_dir: PathBuf,
    pub(crate) space: String,
    pub(crate) agent: String,
    pub(crate) offline: bool,
    /// Host-configured share state directory; `None` reads no shared facts.
    pub(crate) share_state_dir: Option<PathBuf>,
}

struct EmbedderLoad {
    #[cfg(feature = "model2vec")]
    embedder: Option<Model2VecEmbedder>,
    warning: Option<String>,
}

impl EmbedderLoad {
    /// No model, because none was needed — not because one failed to load.
    fn none() -> Self {
        Self {
            #[cfg(feature = "model2vec")]
            embedder: None,
            warning: None,
        }
    }

    fn as_dyn(&self) -> Option<&dyn Embedder> {
        #[cfg(feature = "model2vec")]
        {
            self.embedder
                .as_ref()
                .map(|embedder| embedder as &dyn Embedder)
        }
        #[cfg(not(feature = "model2vec"))]
        {
            None
        }
    }
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Printing the error ourselves rather than returning it from main
            // keeps the internal type out of the message: the default handler
            // renders it with Debug, so a storage failure reached the user as
            // Sqlite(SqliteFailure(Error { code: ReadOnly, extended_code: 8 }))
            // — accurate, and no help to anyone.
            eprintln!("Error: {}", humanize(error.as_ref()));
            std::process::ExitCode::FAILURE
        }
    }
}

/// Rewrites the failures a person is most likely to hit into advice.
///
/// Anything unrecognised passes through unchanged: inventing an explanation
/// for an error nobody anticipated would be worse than showing it verbatim.
fn humanize(error: &dyn Error) -> String {
    let text = error.to_string();
    let hint = if text.contains("readonly") {
        "the database is read-only; check the file's permissions"
    } else if text.contains("no such table") || text.contains("not a database") {
        "this file is not a Brain database; check --database, or run `brain init`"
    } else if text.contains("database is locked") {
        "the database is busy with another Brain command; try again shortly"
    } else if text.contains("disk I/O error") {
        "the disk refused the operation; check free space and permissions"
    } else if text.contains("no space left") {
        "the disk is full"
    } else {
        return text;
    };
    format!("{hint} ({text})")
}

/// Plug-in LLM providers ("compute nodes").
#[derive(Debug, Subcommand)]
enum LlmCommand {
    /// List configured providers in fallback order.
    List,
    /// Add a provider; the API key stays in an environment variable.
    Add {
        /// Provider name (referenced by llm test/remove).
        name: String,
        /// Base URL: https://… or http://127.0.0.1[:port].
        #[arg(long)]
        base_url: Option<String>,
        /// Model name as the provider expects it.
        #[arg(long)]
        model: Option<String>,
        /// Built-in subscription CLI adapter (no shell).
        #[arg(long, value_parser = ["claude", "codex"], conflicts_with_all = ["base_url", "api_key_env"])]
        command: Option<String>,
        /// Reasoning effort for the adapter (low, medium or high; requires --command).
        #[arg(long, value_parser = ["low", "medium", "high"], requires = "command")]
        effort: Option<String>,
        /// Environment variable NAME holding the API key (never the key).
        #[arg(long)]
        api_key_env: Option<String>,
        /// Fallback order: lower is tried first (default 100).
        #[arg(long)]
        priority: Option<u32>,
        /// Context window including output (HTTP default 16384; command default 128000).
        #[arg(long)]
        context_tokens: Option<u32>,
        /// Request timeout for this provider (10..=3600 seconds; default 120).
        #[arg(long)]
        timeout_seconds: Option<u64>,
    },
    /// Remove a provider by name.
    Remove { name: String },
    /// Send a short fixed request and print availability/latency counters.
    Test {
        /// Provider name; omitted = all configured, in fallback order.
        name: Option<String>,
        /// Seconds until the request times out (default 20).
        #[arg(long, default_value_t = 20)]
        timeout_seconds: u64,
        /// Print the response text (default: counters only).
        #[arg(long)]
        show: bool,
    },
}

/// Distillation commands.
#[derive(Debug, Subcommand)]
enum DistillCommand {
    /// Extract and persist facts from visible archive sessions.
    Run {
        /// Distill only this archive session id.
        #[arg(long)]
        session: Option<String>,
        /// Skip sessions whose latest entry is older than this ISO date or Unix timestamp.
        #[arg(long)]
        since: Option<String>,
        /// Comma-separated provider names to use (default: all configured, priority order).
        #[arg(long)]
        provider: Option<String>,
        /// Stop after this many sessions (1-1000, default 20).
        #[arg(long, default_value_t = 20)]
        max_sessions: usize,
        /// Maximum whole sessions per provider request (bounded by actual payload size).
        #[arg(long, default_value_t = 50)]
        batch_sessions: usize,
        /// Process sessions oldest first (default: newest first).
        #[arg(long)]
        oldest_first: bool,
        /// Comma-separated archive roles to include (default user,assistant,tool_error).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "user,assistant,tool_error"
        )]
        roles: Vec<String>,
        /// Count and plan without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Save archive-derived provider responses locally for debugging (private files).
        #[arg(long)]
        keep_raw: Option<PathBuf>,
        /// Internal timer invocation: persist status and suppress progress output.
        #[arg(long, hide = true, conflicts_with = "dry_run")]
        scheduled: bool,
        /// Local HH-HH window in which no provider is invoked (requires --scheduled).
        #[arg(long, requires = "scheduled")]
        quiet_hours: Option<String>,
    },
    /// Manage the opt-in user timer (systemd/launchd).
    Schedule {
        #[command(subcommand)]
        command: distill_schedule::ScheduleCommand,
    },
    /// Show distillation progress and active fact counts.
    Status {
        /// Comma-separated archive roles to include (default user,assistant,tool_error).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "user,assistant,tool_error"
        )]
        roles: Vec<String>,
    },
}
/// Repairs under `brain backup`; each is safe to re-run.
#[derive(Debug, Subcommand)]
enum BackupCommand {
    /// Take a consistent snapshot of brain.sqlite, the archive and settings.
    Create {
        /// Target directory (default: $BRAIN_HOME/backups).
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// List snapshot directories.
    List {
        /// Snapshot directory (default: $BRAIN_HOME/backups).
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Keep only the newest N snapshot directories.
    Prune {
        /// Snapshot directory (default: $BRAIN_HOME/backups).
        #[arg(long)]
        to: Option<PathBuf>,
        /// How many snapshots to keep (default 14, minimum 1).
        #[arg(long, default_value_t = 14)]
        keep: usize,
    },
    /// Replace the live files with a snapshot's contents.
    Restore {
        /// Path to a brain-YYYYMMDD-HHMMSS snapshot directory.
        snapshot: PathBuf,
        /// Apply; without this flag only the plan is shown.
        #[arg(long)]
        yes: bool,
    },
    /// Install or remove the daily backup timer (systemd/launchd user).
    Timer {
        #[arg(value_parser = ["install", "remove"])]
        action: String,
    },
}

#[derive(Debug, Subcommand)]
enum MaintenanceCommand {
    /// Mask secrets already stored in node_versions (history included).
    Redact {
        /// Rewrite the database; without this flag only counters are printed.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SettingsAction {
    /// Print the recent settings changes (who, when, which keys).
    History {
        /// How many recent changes to show (1-10000, default 20).
        #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=10000))]
        limit: u32,
    },
    /// Restore one settings revision (search profile and overrides only).
    Revert {
        /// Exact timestamp from `brain settings history` (Unix nanoseconds).
        #[arg(long)]
        to: String,
        /// Apply the restore; without this flag only the plan is shown.
        #[arg(long)]
        yes: bool,
    },
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    validate_scope(&cli.space, &cli.agent)?;
    if cli
        .share_state_dir
        .as_ref()
        .is_some_and(|dir| !dir.is_absolute())
    {
        return Err("--share-state-dir must be an absolute path".into());
    }
    let database = resolve_database(cli.database.as_deref());
    let config = RuntimeConfig {
        models_dir: resolve_models_dir(cli.models_dir.as_deref()),
        database,
        space: cli.space,
        agent: cli.agent,
        offline: cli.offline,
        share_state_dir: cli.share_state_dir,
    };

    match cli.command {
        Command::Init {
            path,
            agents,
            dry_run,
            yes,
        } => {
            // `--agents` refuses an unusable HOME before the database is
            // created, not only inside the installer. Plain `init` is unchanged.
            if agents {
                private_fs::require_safe_homes(&user_home(), &brain_home())?;
            }
            if !agents || !dry_run {
                command_init(path.as_deref().unwrap_or(&config.database))?;
            }
            if agents {
                let home = user_home();
                let binary = stable_brain_binary()?;
                installer::install(&home, &brain_home(), &binary, dry_run, yes)?;
            }
            Ok(())
        }
        Command::Remember(args) => command_remember(&config, args),
        Command::Ask(args) => command_search(&config, &args, false),
        Command::Recall(args) => command_search(&config, &args, true),
        Command::Index { path, force } => command_index(&config, &path, force),
        Command::When { topic } => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::temporal::when(conn, space_id, as_of, &topic)
        }),
        Command::Timeline { month } => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::temporal::timeline(
                conn,
                space_id,
                as_of,
                brain_core::temporal::TimelineArgs {
                    month: month.clone(),
                    from: None,
                    to: None,
                },
            )
        }),
        Command::Explain { decision } => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::temporal::explain(conn, space_id, as_of, &decision)
        }),
        Command::Blockers { limit } => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::operations::blockers(conn, space_id, as_of.recorded_at, limit)
                .map_err(|error| error.to_string())
        }),
        Command::Checkpoint => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::operations::checkpoint(conn, space_id, as_of.recorded_at)
                .map_err(|error| error.to_string())
        }),
        Command::Goals => command_temporal(&config, |conn, space_id, as_of| {
            brain_core::goals::goals(conn, space_id, as_of.recorded_at)
                .map_err(|error| error.to_string())
        }),
        Command::Goal { action } => match action {
            GoalAction::Set { text } => {
                command_on_database(&config, true, |conn, space_id, as_of| {
                    brain_core::goals::set(conn, space_id, &text, as_of.recorded_at)
                        .map_err(|error| error.to_string())
                })
            }
            GoalAction::Clear => command_on_database(&config, true, |conn, space_id, as_of| {
                brain_core::goals::clear(conn, space_id, as_of.recorded_at)
                    .map_err(|error| error.to_string())
            }),
        },
        Command::Config { action } => command_config(action),
        Command::Settings { action } => match action {
            SettingsAction::History { limit } => print_json(&json!(settings_history::history(
                &brain_home(),
                limit as usize
            )?)),
            SettingsAction::Revert { to, yes } => {
                settings_history::revert(&brain_home(), &to, yes)?;
                print_json(
                    &json!({"status": "ok", "reverted_to": to, "scope": "profile/overrides"}),
                )
            }
        },
        Command::Status => command_status(&config),
        Command::Stats => print_json(&stats::document(
            &config.database,
            &config.space,
            &archive::status(&brain_home(), &user_home()),
            now_unix_seconds(),
        )?),
        Command::Backup { command } => match command {
            BackupCommand::Create { to } => print_json(&backup::create(&brain_home(), to)?),
            BackupCommand::List { to } => print_json(&backup::list(&brain_home(), to)?),
            BackupCommand::Prune { to, keep } => {
                print_json(&backup::prune(&brain_home(), to, keep)?)
            }
            BackupCommand::Restore { snapshot, yes } => {
                print_json(&backup::restore(&brain_home(), &snapshot, yes)?)
            }
            BackupCommand::Timer { action } => {
                let install = action == "install";
                let binary = stable_brain_binary()?;
                #[cfg(target_os = "macos")]
                let mac_result = {
                    let args = vec![
                        binary.to_str().ok_or("binary is not UTF-8")?.into(),
                        "backup".into(),
                        "create".into(),
                    ];
                    crate::scheduler::manage(
                        &user_home(),
                        crate::scheduler::BACKUP,
                        install.then_some((args.as_slice(), 86400)),
                    )?
                };
                #[cfg(target_os = "linux")]
                let (files, switched, reloaded) = {
                    let systemctl = |args: &[&str]| {
                        std::process::Command::new("systemctl")
                            .arg("--user")
                            .args(args)
                            .stdin(std::process::Stdio::null())
                            .status()
                            .is_ok_and(|status| status.success())
                    };
                    if install {
                        let files = backup::write_timer_units(&user_home(), &binary, true)?;
                        let reloaded = systemctl(&["daemon-reload"]);
                        (
                            files,
                            systemctl(&["enable", "--now", backup::TIMER_UNIT]),
                            reloaded,
                        )
                    } else {
                        let switched = systemctl(&["disable", "--now", backup::TIMER_UNIT]);
                        let files = backup::write_timer_units(&user_home(), &binary, false)?;
                        (files, switched, systemctl(&["daemon-reload"]))
                    }
                };
                let stored = settings::load(&brain_home());
                let mut stored = stored;
                stored.backup_timer_enabled = install;
                settings::save(&brain_home(), &stored)?;
                #[cfg(target_os = "macos")]
                print_json(&mac_result)?;
                #[cfg(target_os = "linux")]
                print_json(&json!({
                    "status": "ok",
                    "action": action,
                    "unit_files": files,
                    "systemctl_switched": switched,
                    "systemctl_reloaded": reloaded,
                    "note": "systemd user units; the timer must be enabled by systemd",
                }))?;
                Ok(())
            }
        },
        Command::Maintenance { command } => match command {
            MaintenanceCommand::Redact { apply } => {
                let document = maintenance::redact_document(&config.database, apply)?;
                print_json(&document)?;
                Ok(())
            }
        },
        Command::Llm { command } => match command {
            LlmCommand::List => {
                let stored = settings::load(&brain_home());
                print_json(&llm::list_document(&stored.llm_providers))?;
                Ok(())
            }
            LlmCommand::Add {
                command,
                effort,
                name,
                base_url,
                model,
                api_key_env,
                priority,
                context_tokens,
                timeout_seconds,
            } => {
                let mut fields: Vec<(String, String)> = vec![
                    ("name".to_owned(), name),
                    ("base_url".to_owned(), base_url.unwrap_or_default()),
                    ("model".to_owned(), model.unwrap_or_default()),
                ];
                if let Some(command) = command {
                    fields.push(("command".into(), command));
                }
                if let Some(effort) = effort {
                    fields.push(("effort".into(), effort));
                }
                if let Some(api_key_env) = api_key_env {
                    fields.push(("api_key_env".to_owned(), api_key_env));
                }
                if let Some(priority) = priority {
                    fields.push(("priority".to_owned(), priority.to_string()));
                }
                if let Some(tokens) = context_tokens {
                    fields.push(("context_tokens".into(), tokens.to_string()));
                }
                if let Some(seconds) = timeout_seconds {
                    fields.push(("timeout_seconds".into(), seconds.to_string()));
                }
                let provider = llm::llm_provider_from_fields(&fields)?;
                let mut stored = settings::load(&brain_home());
                if stored
                    .llm_providers
                    .iter()
                    .any(|existing| existing.name == provider.name)
                {
                    return Err(format!(
                        "provider {:?} already exists; remove it first",
                        provider.name
                    )
                    .into());
                }
                stored.llm_providers.push(provider);
                settings::save(&brain_home(), &stored)?;
                print_json(&json!({
                    "status": "saved",
                    "note": "the API key itself is never stored; only the environment variable name",
                }))?;
                Ok(())
            }
            LlmCommand::Remove { name } => {
                let mut stored = settings::load(&brain_home());
                let before = stored.llm_providers.len();
                stored.llm_providers.retain(|p| p.name != name);
                if stored.llm_providers.len() == before {
                    return Err(format!("provider {name:?} is not configured").into());
                }
                settings::save(&brain_home(), &stored)?;
                print_json(&json!({"status": "removed", "name": name}))?;
                Ok(())
            }
            LlmCommand::Test {
                name,
                timeout_seconds,
                show: _,
            } => {
                let stored = settings::load(&brain_home());
                let mut providers: Vec<llm::LlmProvider> = stored
                    .llm_providers
                    .iter()
                    .filter(|p| name.as_deref().is_none_or(|want| p.name == want))
                    .cloned()
                    .collect();
                if name.is_some() && providers.is_empty() {
                    return Err(format!("provider {name:?} is not configured").into());
                }
                providers.sort_by_key(|p| p.priority);
                if providers.is_empty() {
                    print_json(&json!({
                        "status": "empty",
                        "note": "no providers configured; use brain llm add",
                    }))?;
                    return Ok(());
                }
                let timeout = std::time::Duration::from_secs(timeout_seconds.max(1));
                let prompt = r#"Reply with exactly this JSON and nothing else: {"ok": true}"#;
                let schema = json!({
                    "type": "object",
                    "properties": {"ok": {"type": "boolean"}},
                    "required": ["ok"],
                });
                let (attempts, winner) = llm::chat_with_fallback(
                    &providers,
                    prompt,
                    64,
                    0.0,
                    Some(&schema),
                    Some(timeout),
                );
                let attempts_json: Vec<Value> = attempts
                    .iter()
                    .map(|attempt| {
                        json!({
                            "provider": attempt.provider,
                            "model": attempt.model,
                            "ok": attempt.ok,
                            "elapsed_ms": attempt.elapsed_ms,
                            "tokens_per_second": attempt.tokens_per_second,
                            "json_valid": attempt.json_valid,
                            "error": attempt.error,
                        })
                    })
                    .collect();
                print_json(&json!({
                    "status": if winner.is_some() { "ok" } else { "unavailable" },
                    "attempts": attempts_json,
                    "answered_by": winner.as_ref().map(|w| json!({
                        "provider": w.provider,
                        "model": w.model,
                        "elapsed_ms": w.elapsed_ms,
                        "tokens_per_second": w.tokens_per_second,
                        "json_valid": w.json_valid,
                    })),
                    "note": "the response text is not printed unless --show; API keys are read from the environment and never printed",
                }))?;
                Ok(())
            }
        },
        Command::Distill { command } => match command {
            DistillCommand::Run {
                session,
                since,
                provider,
                max_sessions,
                batch_sessions,
                oldest_first,
                roles,
                dry_run,
                keep_raw,
                scheduled,
                quiet_hours,
            } => print_json(&distill_schedule::run(
                &config,
                distill::RunArgs {
                    session,
                    since,
                    provider,
                    max_sessions,
                    batch_sessions,
                    oldest_first,
                    roles,
                    dry_run,
                    keep_raw,
                    quiet: scheduled,
                },
                scheduled,
                quiet_hours.as_deref(),
            )?),
            DistillCommand::Schedule { command } => {
                print_json(&distill_schedule::command(&config, command)?)
            }
            DistillCommand::Status { roles } => print_json(&distill::status(&config, roles)?),
        },
        Command::Query { question, rest } => run_graphify("query", &[question], &rest),
        Command::Path { from, to, rest } => run_graphify("path", &[from, to], &rest),
        Command::Graph {
            command: GraphCommand::Explain { node, rest },
        } => run_graphify("explain", &[node], &rest),
        Command::Doctor => print_json(&doctor::document(
            &user_home(),
            &brain_home(),
            now_unix_seconds(),
        )),
        Command::Memory { command } => command_memory(&config, command),
        Command::Serve { remote_config } => {
            if let Some(path) = remote_config {
                remote_mcp::install(&path)?;
            }
            mcp::run_stdio(&config)
        }
        Command::Archive { command } => command_archive(command),
        Command::Work { command } => work::run(&config, command),
        Command::Map { command } => map::run(&config, command),
        Command::Net { command } => net::run(&config, command),
        Command::Team { command } => net::team::run(&config, command),
        Command::Hook(args) => Ok(hook::run(&args, &brain_home())?),
        Command::Brief(args) => command_brief(&config, &args),
        Command::Model { command } => command_model(&config, command),
        Command::Similar { query } => command_experience(&config, &query, false),
        Command::Template { query } => command_experience(&config, &query, true),
        Command::Ui(args) => ui::run(&config, &args),
        Command::Reindex(args) => reindex::run(&config, &args),
        Command::Import(args) => import::run(&config, &args),
        Command::Clients { action } => print_json(&client_registry::run(
            &action,
            &user_home(),
            &brain_home(),
            now_unix_seconds(),
        )?),
        Command::Uninstall { dry_run, yes } => {
            installer::uninstall(&user_home(), &brain_home(), dry_run, yes)?;
            Ok(())
        }
        Command::Worker => {
            print_json(&command_worker(&brain_home())?)?;
            Ok(())
        }
        Command::ModelDownload { model_id } => command_model_download(&config, &model_id),
    }
}

fn command_archive(command: ArchiveCommand) -> Result<(), Box<dyn Error>> {
    let brain_home = brain_home();
    match command {
        ArchiveCommand::Status => print_json(&archive::status(&brain_home, &user_home())),
        ArchiveCommand::Search {
            query,
            client,
            role,
            session,
            project_root,
            limit,
            budget_tokens,
        } => {
            let request = archive::SearchRequest {
                query,
                client,
                role,
                session_id: session,
                project_root,
                limit,
                budget_tokens,
                match_any: false,
                period: None,
            };
            print_json(&archive::search(&brain_home, &request)?)
        }
        ArchiveCommand::Export {
            format,
            session,
            since,
            until,
            no_content,
        } => {
            if format != "otlp-json" {
                return Err(format!("unsupported export format {format:?}; only otlp-json").into());
            }
            let since = since
                .as_deref()
                .map(crate::mcp::parse_iso8601_or_unix)
                .transpose()?;
            let until = until
                .as_deref()
                .map(crate::mcp::parse_iso8601_or_unix)
                .transpose()?;
            print_json(&otel_export::export_otlp_json(
                &brain_home,
                session.as_deref(),
                since,
                until,
                no_content,
            )?)?;
            Ok(())
        }
        ArchiveCommand::Read {
            id,
            offset,
            max_bytes,
            context,
        } => {
            let request = archive::ReadRequest {
                id,
                offset,
                max_bytes,
                context,
            };
            print_json(&archive::read(&brain_home, &request)?)
        }
        ArchiveCommand::Collect { quiet } => {
            let result = archive::collect(&brain_home, &user_home())?;
            if quiet {
                let inserted: u64 = result["clients"]
                    .as_object()
                    .map(|clients| {
                        clients
                            .values()
                            .filter_map(|c| c["inserted"].as_u64())
                            .sum()
                    })
                    .unwrap_or(0);
                println!(
                    "archive collect: status={} visited={} inserted={} stopped_by={}",
                    result["status"].as_str().unwrap_or("?"),
                    result["visited"].as_u64().unwrap_or(0),
                    inserted,
                    result["stopped_by"].as_str().unwrap_or("none"),
                );
                Ok(())
            } else {
                print_json(&result)
            }
        }
        ArchiveCommand::Enable {
            since_days,
            clients,
            roles,
        } => print_json(&archive::set_enabled(
            &brain_home,
            true,
            since_days,
            clients.as_deref(),
            roles.as_deref(),
        )?),
        ArchiveCommand::Disable => {
            print_json(&archive::set_enabled(&brain_home, false, None, None, None)?)
        }
        ArchiveCommand::Backfill { action } => match action {
            BackfillCommand::Status => print_json(&archive::backfill_document(&brain_home)),
            BackfillCommand::Start { files_per_run } => {
                print_json(&archive::backfill_start(&brain_home, files_per_run)?)
            }
            BackfillCommand::Stop => print_json(&archive::backfill_stop(&brain_home)?),
        },
        ArchiveCommand::Timer { action } => {
            let install = action == "install";
            let binary = stable_brain_binary()?;
            #[cfg(target_os = "macos")]
            {
                let args = vec![
                    binary.to_str().ok_or("binary is not UTF-8")?.into(),
                    "archive".into(),
                    "collect".into(),
                    "--quiet".into(),
                ];
                print_json(&crate::scheduler::manage(
                    &user_home(),
                    crate::scheduler::ARCHIVE,
                    install.then_some((args.as_slice(), 120)),
                )?)
            }
            #[cfg(target_os = "linux")]
            {
                let systemctl = |args: &[&str]| {
                    std::process::Command::new("systemctl")
                        .arg("--user")
                        .args(args)
                        .stdin(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|status| status.success())
                };
                let (files, switched, reloaded) = if install {
                    let files = archive::write_timer_units(&user_home(), &binary, true)?;
                    let reloaded = systemctl(&["daemon-reload"]);
                    (
                        files,
                        systemctl(&["enable", "--now", archive::TIMER_UNIT]),
                        reloaded,
                    )
                } else {
                    // Stop and disable while the unit still exists, then remove its files.
                    let switched = systemctl(&["disable", "--now", archive::TIMER_UNIT]);
                    let files = archive::write_timer_units(&user_home(), &binary, false)?;
                    (files, switched, systemctl(&["daemon-reload"]))
                };
                print_json(&json!({
                    "status": if reloaded && switched { "ok" } else { "files_written_systemctl_failed" },
                    "action": action,
                    "unit_files": files,
                    "systemctl_daemon_reload": reloaded,
                    "systemctl_enable_or_disable": switched,
                }))
            }
        }
    }
}

fn command_init(database: &Path) -> Result<(), Box<dyn Error>> {
    let storage = open_storage(database)?;
    let output = json!({
        "status": "initialized",
        "database": database,
        "schema_version": storage.schema_version()?,
    });
    print_json(&output)?;
    Ok(())
}

fn command_brief(config: &RuntimeConfig, args: &BriefArgs) -> Result<(), Box<dyn Error>> {
    let output = brief::build(config, args.cwd.as_deref(), args.git_branch.as_deref())?;
    print_json(&json!({
        "status": "ok",
        "tokens_used": output.tokens_used,
        "token_estimator": "utf8-bytes-upper-bound",
        "text": output.text,
        "sections": output.sections,
    }))?;
    Ok(())
}

fn command_model(config: &RuntimeConfig, command: ModelCommand) -> Result<(), Box<dyn Error>> {
    match command {
        ModelCommand::Install {
            model,
            timeout_seconds,
            yes,
        } => command_model_install(config, model, timeout_seconds, yes),
    }
}

fn command_model_install(
    config: &RuntimeConfig,
    model: ModelChoice,
    timeout_seconds: u64,
    yes: bool,
) -> Result<(), Box<dyn Error>> {
    if config.offline {
        return Err("--offline forbids explicit model downloads".into());
    }
    if timeout_seconds == 0 {
        return Err("--timeout-seconds must be greater than zero".into());
    }
    let model_id = model.model_id();
    let bytes = background::model_download_bytes(model_id);
    #[cfg(feature = "model2vec")]
    if model_is_cached(model_id, &config.models_dir) {
        let (worker_started, worker_warning) = resume_installed_backlog(config, model_id);
        print_json(&json!({
            "status": "already_installed",
            "model": model_id,
            "models_dir": config.models_dir,
            "embedding_mode": embedding_runtime::stored_mode(&brain_home()).to_string(),
            "backlog_worker_started": worker_started,
            "warning": worker_warning,
        }))?;
        return Ok(());
    }
    #[cfg(not(feature = "model2vec"))]
    {
        let _ = (model_id, bytes, yes);
        Err("embedding support was not compiled into this binary (build with the default features, not --no-default-features)".into())
    }

    #[cfg(feature = "model2vec")]
    {
        if !yes {
            eprint!(
                "Download local embedding model {model_id} (about {} MB)? [y/N] ",
                bytes / 1_000_000
            );
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                print_json(&json!({"status": "cancelled", "model": model_id}))?;
                return Ok(());
            }
        }

        let executable = stable_brain_binary()?;
        let mut child = ProcessCommand::new(executable)
            .arg("--models-dir")
            .arg(&config.models_dir)
            .arg("__model-download")
            .arg("--model-id")
            .arg(model_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(format!("model downloader exited with {status}").into());
                }
                break;
            }
            if started.elapsed() >= Duration::from_secs(timeout_seconds) {
                child.kill()?;
                let _ = child.wait();
                return Err(format!(
                    "model download exceeded {timeout_seconds}s and was stopped; rerun the command to resume"
                )
                .into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        let (worker_started, worker_warning) = resume_installed_backlog(config, model_id);
        print_json(&json!({
            "status": "installed",
            "model": model_id,
            "advertised_bytes": bytes,
            "models_dir": config.models_dir,
            "resume": "enabled",
            "timeout_seconds": timeout_seconds,
            "embedding_mode": embedding_runtime::stored_mode(&brain_home()).to_string(),
            "backlog_worker_started": worker_started,
            "warning": worker_warning,
        }))?;
        Ok(())
    }
}

/// Starts old queued work after an explicit install only when `always` asks for it.
///
/// A failed worker start must not turn a completed model installation into a
/// failure, but it must be visible in the machine-readable result.
#[cfg(feature = "model2vec")]
fn resume_installed_backlog(
    config: &RuntimeConfig,
    model_id: &'static str,
) -> (bool, Option<String>) {
    let home = brain_home();
    let mode = embedding_runtime::stored_mode(&home);
    let plan = embedding_runtime::SemanticPlan::for_model(mode, model_id, &config.models_dir);
    if !plan.indexes_writes_eagerly()
        || embedding_runtime::pending_vector_jobs(&home, &config.database) == 0
    {
        return (false, None);
    }
    match background::spawn_worker() {
        Ok(()) => (true, None),
        Err(error) => (
            false,
            Some(format!(
                "model installed, but the queued vector worker could not start: {error}"
            )),
        ),
    }
}

#[cfg(feature = "model2vec")]
fn command_model_download(config: &RuntimeConfig, model_id: &str) -> Result<(), Box<dyn Error>> {
    install_model(model_id, &config.models_dir)?;
    Ok(())
}

#[cfg(not(feature = "model2vec"))]
fn command_model_download(_config: &RuntimeConfig, _model_id: &str) -> Result<(), Box<dyn Error>> {
    Err("embedding support was not compiled into this binary (build with the default features, not --no-default-features)".into())
}

fn command_remember(config: &RuntimeConfig, args: RememberArgs) -> Result<(), Box<dyn Error>> {
    let now = now_unix_seconds();
    let home = brain_home();
    let plan = embedding_runtime::SemanticPlan::current(&home, &args.text, &config.models_dir);
    let storage = open_storage(&config.database)?;
    let attribution = ensure_context(&storage, config, now)?;
    let service = MemoryService::without_vectors(&storage, plan.write_note());
    let mut output = service.remember(&RememberRequest {
        text: args.text.clone(),
        kind: args.kind.parse::<MemoryKind>()?,
        tags: args.tags,
        space_id: config.space.clone(),
        source: args.source,
        valid_from: args.valid_from.unwrap_or(now),
        valid_to: args.valid_to,
        attribution,
        source_weight: args.source_weight,
    })?;
    match background::enqueue_embedding(
        &home,
        &config.database,
        &config.models_dir,
        &config.space,
        &config.agent,
        &output.id,
        &args.text,
    ) {
        Ok(()) => {
            if plan.indexes_writes_eagerly()
                && let Err(error) = background::spawn_worker()
            {
                output
                    .warnings
                    .push(format!("background vector worker could not start: {error}"));
            }
        }
        Err(error) => output
            .warnings
            .push(format!("background vector queue failed: {error}")),
    }
    emit_warnings(&output.warnings);
    print_json(&json!({
        "status": "remembered",
        "id": output.id,
        "duplicates": output.duplicates.iter().map(|duplicate| json!({
            "id": duplicate.id,
            "similarity": duplicate.similarity,
        })).collect::<Vec<_>>(),
        "contradictions": output.contradictions.iter().map(|contradiction| json!({
            "existing_id": contradiction.existing_id,
            "existing_text": contradiction.existing_text,
            "score": contradiction.score,
            "suggestion": contradiction.suggestion,
        })).collect::<Vec<_>>(),
        "vector_indexed": output.vector_indexed,
        "warnings": output.warnings,
    }))?;
    Ok(())
}

fn command_search(
    config: &RuntimeConfig,
    args: &SearchArgs,
    memory_only: bool,
) -> Result<(), Box<dyn Error>> {
    let now = now_unix_seconds();
    let storage = open_storage(&config.database)?;
    let layers = if args.all_layers {
        SearchLayers::ALL
    } else {
        SearchLayers::HOT
    };
    let mut query =
        SearchQuery::new(&config.space, &args.query, AsOf::new(now, now)).with_layers(layers);
    if memory_only {
        query = query.memory_only();
    }
    let stored = settings::load(&brain_home());
    // A profile named on the command line replaces the stored one outright,
    // hand-tuned knobs included: asking for `--profile fast` and silently
    // keeping someone's 2000-candidate override would not be that profile.
    let (profile, mut quality) = match args.profile.as_deref() {
        Some(name) => {
            let profile = name
                .parse::<brain_core::profile::Profile>()
                .map_err(Box::<dyn Error>::from)?;
            (profile, profile.settings())
        }
        None => (stored.profile(), stored.effective()),
    };
    // An explicit budget always wins over the profile default.
    if let Some(budget) = args.budget_tokens {
        quality.budget_tokens = budget;
    }

    // Loading the model costs about 2.5 seconds against 0.4 for the whole
    // lexical search, because 507 MB of vectors are copied out of the file. In
    // "on demand" that price is only worth paying when the words alone did not
    // answer — which is what makes the setting mean something. It used to load
    // the model up front whatever the mode said, so "always in memory" and "on
    // demand" measured 3.0 and 2.8 seconds: indistinguishable, and the switch
    // was decoration.
    let deferred = settings::load(&brain_home()).embedding_mode
        == settings::EmbeddingMode::OnDemand
        && !args.semantic;
    let lexical_only = HybridSearch::without_vectors(&storage, "");
    let first = deferred
        .then(|| lexical_only.search_with(&query, &quality))
        .transpose()?;
    let (output, loaded) = match first {
        Some(found) if lexical_answer_is_enough(&found, &quality) => (found, EmbedderLoad::none()),
        _ => {
            let loaded = load_cached_embedder(&args.query, config);
            let search = HybridSearch::with_optional_embedder(
                &storage,
                loaded.as_dyn(),
                loaded.warning.clone(),
            );
            (search.search_with(&query, &quality)?, loaded)
        }
    };
    let mut warnings = output.warnings.clone();
    let results = output
        .hits
        .iter()
        .map(|hit| {
            let memory_kind = MemoryRecord::from_node(hit.node.clone())
                .ok()
                .map(|memory| memory.kind.to_string());
            let metadata: Value = serde_json::from_str(&hit.node.metadata_json).unwrap_or_default();
            let distilled_kind = metadata.get("distilled_fact").and_then(Value::as_bool)
                .filter(|v| *v).and_then(|_| metadata.get("kind").and_then(Value::as_str));
            json!({
                "id": hit.node.id,
                "type": distilled_kind,
                "status": metadata.get("status"),
                "kind": distilled_kind.map(str::to_owned).or(memory_kind).unwrap_or_else(|| hit.node.kind.to_string()),
                "text": hit.node.body,
                "source": hit.node.source,
                "score": hit.score,
                "token_len": hit.node.token_len,
                "valid_from": hit.node.valid_from,
                "valid_to": hit.node.valid_to,
                "source_agent": hit.node.agent_id,
                "signals": {
                    "bm25": hit.matched_bm25,
                    "vector": hit.matched_vector,
                    "exact": hit.matched_exact,
                    "graph_hops_away": hit.graph_distance,
                },
            })
        })
        .collect::<Vec<_>>();
    let parsed = when_phrase::parse(&args.query, now);
    let mut archive = if memory_only {
        ArchiveSearch::default()
    } else {
        // The role restriction is the caller's own choice, unlike the hygiene
        // filters inside search_conversations, so a typo is an error rather
        // than a silent empty archive.
        for role in &args.archive_role {
            if !archive::ROLES.contains(&role.as_str()) {
                return Err(format!(
                    "--archive-role must be one of {}",
                    archive::ROLES.join(", ")
                )
                .into());
            }
        }
        let mut found = search_conversations(
            &brain_home(),
            &args.query,
            stored.include_archive && !args.no_archive,
            stored.archive_min_age_minutes,
            // Asking the archive for the period itself: filtering 20 all-time
            // candidates afterwards leaves one or two, chosen by other dates.
            parsed.as_ref().map(|(period, _)| (period.from, period.to)),
        );
        if !args.archive_role.is_empty() {
            found.results.retain(|hit| {
                hit["role"]
                    .as_str()
                    .is_some_and(|role| args.archive_role.iter().any(|want| want.as_str() == role))
            });
        }
        found
    };
    let (results, period_json, period_empty, period_hint) =
        apply_question_period(&args.query, parsed.as_ref(), &mut archive, results);
    warnings.extend(archive.warnings.iter().cloned());
    let archive_included = archive.included;
    let archive_filtered = json!({
        "recent": archive.filtered_recent,
        "self_reference": archive.filtered_self_reference,
    });
    let (results, used_tokens, archive_hits) = fuse_conversations(
        results,
        archive,
        quality.budget_tokens,
        usize::MAX,
        ArchiveRanking::for_question(&args.query, parsed.is_some()),
    );
    emit_warnings(&warnings);
    let hint = if let Some(hint) = period_hint {
        Some(hint)
    } else if results.is_empty() && loaded.as_dyn().is_none() {
        loaded.warning.clone().or_else(|| {
            Some("semantic search is unavailable; try broader BM25 or exact terms".to_owned())
        })
    } else {
        None
    };
    print_json(&json!({
        "status": if results.is_empty() { "empty" } else { "ok" },
        "scope": if memory_only { "memory" } else { "all" },
        "query": args.query,
        "found": results.len(),
        "results": results,
        "budget_tokens": quality.budget_tokens,
        "profile": profile.to_string(),
        "candidates_per_signal": quality.candidates_per_signal,
        "used_tokens": used_tokens,
        "period": period_json,
        "period_empty": period_empty,
        "archive_included": archive_included,
        "archive_hits": archive_hits,
        "archive_filtered": archive_filtered,
        "token_estimator": "utf8-bytes-upper-bound",
        "vector_search": output.vector_search,
        "graph_reached": output.graph_reached,
        "warnings": warnings,
        "hint": hint,
    }))?;
    Ok(())
}

/// Archive data stays outside Brain spaces and is always untrusted evidence.
#[derive(Default)]
struct ArchiveSearch {
    results: Vec<Value>,
    included: bool,
    warnings: Vec<String>,
    filtered_recent: usize,
    filtered_self_reference: usize,
}

/// Drops archive hits that restate the question back at the asker.
///
/// The conversation an agent is having right now is collected within minutes
/// and quotes the very `brain ask "…"` invocation that triggered the search,
/// so without these gates its own messages outrank the weeks-old fact the
/// question is about. `recent` counts hits younger than `min_age_minutes`
/// (age by event time, falling back to when the collector observed the entry;
/// an entry whose time cannot be read counts as brand new); `self_reference`
/// counts hits quoting the search command itself, whatever their age.
fn filter_archive_hits(
    hits: Vec<Value>,
    min_age_minutes: u32,
    now: i64,
) -> (Vec<Value>, usize, usize) {
    let min_age_seconds = i64::from(min_age_minutes) * 60;
    let mut kept = Vec::with_capacity(hits.len());
    let mut recent = 0;
    let mut self_reference = 0;
    for hit in hits {
        if min_age_seconds > 0 {
            let event = hit["timestamp"]
                .as_str()
                .and_then(archive::epoch_seconds)
                .or_else(|| hit["observed_at"].as_str().and_then(archive::epoch_seconds));
            let age = event.map_or(0, |seconds| now - seconds);
            if age < min_age_seconds {
                recent += 1;
                continue;
            }
        }
        let text = hit["excerpt"].as_str().unwrap_or_default();
        // The search quoting itself: typed, run from a tool call (quotes escaped), or
        // its own JSON output read back from a tool result.
        if text.contains("brain ask")
            || text.contains("brain recall")
            || text.contains("archive_hits")
        {
            self_reference += 1;
            continue;
        }
        kept.push(hit);
    }
    (kept, recent, self_reference)
}

fn search_conversations(
    home: &Path,
    query: &str,
    enabled: bool,
    min_age_minutes: u32,
    period: Option<(i64, i64)>,
) -> ArchiveSearch {
    let mut found = ArchiveSearch::default();
    let disabled = if !enabled {
        Some("conversation archive search disabled by include_archive or --no-archive")
    } else {
        match archive::collector_switch(home) {
            "archive_collector_paused" => {
                Some("conversation archive is disabled; using core results")
            }
            "archive_config_invalid" => {
                Some("conversation archive configuration is invalid; using core results")
            }
            _ => None,
        }
    };
    if let Some(warning) = disabled {
        found.warnings.push(warning.to_owned());
        return found;
    }
    // Fetch a bounded candidate window; fusion below applies the user's smaller
    // quota. archive::search's public API requires at least 500 tokens.
    match archive::search(
        home,
        &archive::SearchRequest {
            query: query.to_owned(),
            limit: Some(20),
            budget_tokens: Some(4000),
            match_any: true,
            period,
            ..archive::SearchRequest::default()
        },
    ) {
        Ok(document) => {
            found.included = true;
            let hits = document["results"].as_array().cloned().unwrap_or_default();
            let (kept, recent, self_reference) =
                filter_archive_hits(hits, min_age_minutes, now_unix_seconds());
            found.results = conversation_first(distinct_excerpts(kept));
            found.filtered_recent = recent;
            found.filtered_self_reference = self_reference;
        }
        Err(error) => found.warnings.push(format!(
            "conversation archive unavailable: {error}; using core results"
        )),
    }
    found
}

/// One hit per (client, role, excerpt): a Claude session repeats the same
/// thinking block on several transcript lines, and each copy would take one
/// of the few archive places in the budget. The first (best-ranked) is kept.
fn distinct_excerpts(hits: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|hit| {
            seen.insert((
                hit["client"].as_str().unwrap_or_default().to_owned(),
                hit["role"].as_str().unwrap_or_default().to_owned(),
                hit["excerpt"].as_str().unwrap_or_default().to_owned(),
            ))
        })
        .collect()
}

/// Raw tool calls and tool output rank after what was said, thought or failed.
///
/// Archive hits arrive in bm25 order, and a long tool dump that repeats the
/// question's words outranks the sentence that answers it, while the archive
/// share of the budget holds only a few hits. The order inside each group is kept.
fn conversation_first(hits: Vec<Value>) -> Vec<Value> {
    let (mut said, tools): (Vec<Value>, Vec<Value>) = hits
        .into_iter()
        .partition(|hit| !matches!(hit["role"].as_str(), Some("tool_call" | "tool_result")));
    said.extend(tools);
    said
}

/// Escape URI path components without losing the original provenance fields.
fn archive_uri_component(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Applies the question's date phrase to archive and core hits alike.
///
/// One implementation serves `brain ask` and the MCP `memory_search`: an agent
/// and a person asking the same words must get the same period, the same
/// ranking and the same honest "empty period" answer. Archive hits outside the
/// period are dropped; when the question asks how it began, in-period archive
/// hits go oldest first with conversation ahead of tool traffic; core hits keep
/// their relevance order with in-period ones ahead of the rest. Returns the
/// reordered core results, the `period` JSON value, whether the named period
/// turned out empty, and the hint that must replace the default one.
pub(crate) fn apply_question_period(
    query: &str,
    parsed: Option<&(when_phrase::Period, String)>,
    archive: &mut ArchiveSearch,
    results: Vec<Value>,
) -> (Vec<Value>, Value, bool, Option<String>) {
    let period_json = parsed.as_ref().map_or(
        Value::Null,
        |(period, phrase)| json!({"from": period.from, "to": period.to, "phrase": phrase}),
    );
    let mut results = results;
    let mut in_period = 0;
    if let Some((period, _)) = parsed {
        archive.results.retain(|hit| {
            hit["timestamp"]
                .as_str()
                .and_then(archive::epoch_seconds)
                .is_some_and(|ts| ts >= period.from && ts < period.to)
        });
        in_period = archive.results.len();
        if when_phrase::asks_for_earliest(query) {
            archive.results.sort_by(|a, b| {
                let said_at =
                    |hit: &Value| hit["timestamp"].as_str().and_then(archive::epoch_seconds);
                // What was said comes before raw tool traffic, then oldest first.
                let tool =
                    |hit: &Value| matches!(hit["role"].as_str(), Some("tool_call" | "tool_result"));
                match tool(a).cmp(&tool(b)) {
                    std::cmp::Ordering::Equal => {}
                    other => return other,
                }
                match (said_at(a), said_at(b)) {
                    (Some(left), Some(right)) => left.cmp(&right),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            });
        }
        let mut out_of_period = Vec::new();
        let mut within = Vec::new();
        for hit in results {
            let inside = hit["valid_from"]
                .as_i64()
                .is_some_and(|ts| ts >= period.from && ts < period.to);
            if inside {
                within.push(hit);
            } else {
                out_of_period.push(hit);
            }
        }
        in_period += within.len();
        within.extend(out_of_period);
        results = within;
    }
    let period_empty = parsed.is_some() && in_period == 0;
    let hint = period_empty.then(|| {
        "nothing is stored in the archive for that period; the words match only other dates"
            .to_owned()
    });
    (results, period_json, period_empty, hint)
}

fn command_index(config: &RuntimeConfig, path: &Path, force: bool) -> Result<(), Box<dyn Error>> {
    let root = path
        .canonicalize()
        .map_err(|error| format!("cannot index {}: {error}", path.display()))?;
    let started = Instant::now();
    let (parsed_files, resolved_calls, stats) = brain_core::indexer::index_tree(&root)?;
    let storage = open_storage(&config.database)?;
    let attribution = ensure_context(&storage, config, now_unix_seconds())?;
    let now = now_unix_seconds();
    let mut nodes = Vec::new();
    for file in &parsed_files {
        let file_path = file.path.display().to_string();
        let file_id = format!("code:{file_path}");
        nodes.push(NewNode {
            id: file_id.clone(),
            space_id: config.space.clone(),
            kind: NodeKind::Code,
            label: file
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| file_path.clone()),
            body: file_path.clone(),
            source: Some(file_path.clone()),
            metadata_json: json!({"hash": file.hash, "kind": "file"}).to_string(),
            valid_from: now,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 0.6,
            source_weight: 1.0,
        });
        for function in &file.functions {
            nodes.push(NewNode {
                id: format!("code:{file_path}:{}:{}", function.name, function.line_start),
                space_id: config.space.clone(),
                kind: NodeKind::Code,
                label: function.name.clone(),
                body: format!("{} in {file_path}", function.name),
                source: Some(format!("{file_path}:{}", function.line_start)),
                metadata_json: json!({"kind": "function", "line": function.line_start}).to_string(),
                valid_from: now,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 0.5,
                source_weight: 1.0,
            });
        }
        for class in &file.classes {
            nodes.push(NewNode {
                id: format!("code:{file_path}:{}:{}", class.name, class.line_start),
                space_id: config.space.clone(),
                kind: NodeKind::Code,
                label: class.name.clone(),
                body: format!("class {} in {file_path}", class.name),
                source: Some(format!("{file_path}:{}", class.line_start)),
                metadata_json: json!({"kind": "class", "line": class.line_start}).to_string(),
                valid_from: now,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 0.55,
                source_weight: 1.0,
            });
        }
    }
    let persisted = nodes.len();
    storage.insert_nodes(&nodes)?;
    let node_ids: std::collections::HashSet<String> =
        nodes.iter().map(|node| node.id.clone()).collect();
    let mut edges = Vec::new();
    for call in &resolved_calls {
        let from_id = format!("code:{}", call.from_file.display());
        let to_id = format!(
            "code:{}:{}:{}",
            call.to_file.display(),
            call.name,
            call.to_line
        );
        if !node_ids.contains(&from_id) || !node_ids.contains(&to_id) {
            continue;
        }
        edges.push(brain_core::NewEdge {
            // Line alone collides: `outer(inner())` is two calls on one line,
            // and chained Rust expressions put several there routinely. Without
            // the column, indexing a Rust tree aborts on a uniqueness violation.
            id: format!(
                "calls:{from_id}:{to_id}:{}:{}",
                call.from_line, call.from_column
            ),
            space_id: config.space.clone(),
            from_node_id: from_id,
            to_node_id: to_id,
            relation: "calls".to_string(),
            weight: call.confidence,
            confidence: call.confidence,
            metadata_json: json!({"line": call.from_line}).to_string(),
            valid_from: now,
            valid_to: None,
            attribution: attribution.clone(),
            salience: call.confidence,
        });
    }
    let edges_persisted = edges.len();
    storage.insert_edges(&edges)?;
    storage.flush()?;
    let elapsed_ms = started.elapsed().as_millis();
    print_json(&json!({
        "status": "indexed",
        "root": root.display().to_string(),
        "force": force,
        "files": stats.files,
        "functions": stats.functions,
        "classes": stats.classes,
        "edges": stats.edges,
        "ambiguous_skipped": stats.ambiguous_skipped,
        "parse_errors": stats.parse_errors,
        "elapsed_ms": elapsed_ms,
        "nodes_persisted": persisted,
        "edges_persisted": edges_persisted,
    }))?;
    Ok(())
}

#[derive(Debug, Subcommand)]
enum GoalAction {
    /// Record what this work is aiming at.
    Set {
        /// The goal, in the person's own words.
        text: String,
    },

    /// Withdraw the stated goal and let it be inferred again.
    Clear,
}

#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// Print every quality profile with what it costs and what it buys.
    Show,

    /// Persist a setting so it applies to every later command.
    Set {
        /// Setting name, for example `profile`.
        key: String,
        /// New value.
        value: String,
    },
}

/// Runs a read-only temporal query against the database.
///
/// These queries only read, so they open their own connection instead of going
/// through the write actor.
fn command_temporal<F>(config: &RuntimeConfig, query: F) -> Result<(), Box<dyn Error>>
where
    F: FnOnce(&rusqlite::Connection, &str, AsOf) -> Result<Value, String>,
{
    command_on_database(config, false, query)
}

/// Runs one query against the database, read-only unless it needs to write.
///
/// Read-only is the default because most of these commands only look, and a
/// command that cannot write cannot corrupt. `goal set` is the exception, and
/// routing it through the read-only path made it fail with "the database is
/// read-only" — an accurate message about the wrong thing.
fn command_on_database<F>(
    config: &RuntimeConfig,
    writable: bool,
    query: F,
) -> Result<(), Box<dyn Error>>
where
    F: FnOnce(&rusqlite::Connection, &str, AsOf) -> Result<Value, String>,
{
    let flags = if writable {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    };
    let connection = rusqlite::Connection::open_with_flags(&config.database, flags)?;
    let now = now_unix_seconds();
    let value =
        query(&connection, &config.space, AsOf::new(now, now)).map_err(Box::<dyn Error>::from)?;
    print_json(&value)?;
    Ok(())
}

/// Answers "has this been done before, and how did it go".
///
/// The embedding model, if one is loaded, contributes seeds the lexical search
/// would miss: the core reads SQLite directly and has no embedder of its own,
/// so hits found here would otherwise be thrown away.
fn command_experience(
    config: &RuntimeConfig,
    query: &str,
    as_template: bool,
) -> Result<(), Box<dyn Error>> {
    if !config.database.is_file() {
        return Err(format!(
            "Brain database does not exist or is not a regular file: {}",
            config.database.display()
        )
        .into());
    }
    let now = now_unix_seconds();
    let mut options = brain_core::templates::ExperienceOptions::at(now);
    let loaded = load_cached_embedder(query, config);
    let storage = open_storage(&config.database)?;
    let search =
        HybridSearch::with_optional_embedder(&storage, loaded.as_dyn(), loaded.warning.clone());
    let stored = settings::load(&brain_home());
    let found = search.search_with(
        &SearchQuery::new(&config.space, query, AsOf::new(now, now)).with_layers(SearchLayers::ALL),
        &stored.effective(),
    )?;
    options.degradation_warnings.extend(found.warnings.clone());
    options.trusted_seeds = found
        .hits
        .iter()
        .filter(|hit| hit.graph_distance.is_none())
        .filter_map(|hit| {
            let signal = if hit.matched_exact {
                brain_core::templates::ExperienceSeedSignal::Exact
            } else if hit.matched_vector {
                brain_core::templates::ExperienceSeedSignal::Vector
            } else {
                // A lone BM25 hit still has to pass the core's term-coverage
                // gate. Calling every search result a trusted semantic seed
                // used to turn one shared word into a confident template.
                return None;
            };
            Some(brain_core::templates::ExperienceSeed {
                node_id: hit.node.id.clone(),
                relevance: hit.score.clamp(f64::MIN_POSITIVE, 1.0),
                signal,
            })
        })
        .collect();
    emit_warnings(&loaded.warning.clone().into_iter().collect::<Vec<_>>());

    let connection = rusqlite::Connection::open_with_flags(
        &config.database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let value = if as_template {
        brain_core::templates::template(&connection, &config.space, query, &options)?.to_json()
    } else {
        brain_core::templates::similar(&connection, &config.space, query, &options)?.to_json()
    };
    print_json(&value)?;
    Ok(())
}

/// Reads and writes the same settings document every other channel reads.
///
/// This command used to own a file of its own, so `config set` reported "saved"
/// and `config show` agreed, while search read a different file and ran on a
/// different profile. Anything that reports a setting as stored must store it
/// where the setting is actually read.
fn command_config(action: ConfigAction) -> Result<(), Box<dyn Error>> {
    let home = brain_home();
    match action {
        ConfigAction::Set { key, value } => {
            let mut stored = settings::load(&home);
            match key.as_str() {
                "profile" => {
                    let profile: brain_core::profile::Profile = value.parse()?;
                    stored.profile = profile.to_string();
                }
                // One parser for the CLI and the settings window alike.
                other => settings::apply_memory_setting(&mut stored, other, &value)?,
            }
            settings::save(&home, &stored)?;
            let effective = stored.effective();
            print_json(&json!({
                "status": "saved",
                "profile": stored.profile,
                "include_archive": stored.include_archive,
                "archive_min_age_minutes": stored.archive_min_age_minutes,
                "candidates_per_signal": effective.candidates_per_signal,
                "graph_hops": effective.graph_hops,
                "budget_tokens": effective.budget_tokens,
                "manual_overrides": stored.is_manual(),
                "applies_to": "every later command unless --profile overrides it",
            }))?;
            Ok(())
        }
        ConfigAction::Show => {
            let stored = settings::load(&home);
            let current = stored.profile();
            let effective = stored.effective();
            let profiles: Vec<Value> = brain_core::profile::Profile::all()
                .into_iter()
                .map(|profile| {
                    let settings = profile.settings();
                    json!({
                        "profile": profile.to_string(),
                        "current": profile == current,
                        "candidates_per_signal": settings.candidates_per_signal,
                        "graph_hops": settings.graph_hops,
                        "budget_tokens": settings.budget_tokens,
                        "min_cosine": settings.min_cosine,
                        "min_bm25": settings.min_bm25,
                        "relative_cost": (profile.relative_cost() * 100.0).round() / 100.0,
                    })
                })
                .collect();
            print_json(&json!({
                "status": "ok",
                "current_profile": current.to_string(),
                "manual_overrides": stored.is_manual(),
                "effective": {
                    "candidates_per_signal": effective.candidates_per_signal,
                    "graph_hops": effective.graph_hops,
                    "budget_tokens": effective.budget_tokens,
                    "min_cosine": effective.min_cosine,
                    "min_bm25": effective.min_bm25,
                },
                "embedding_mode": stored.embedding_mode.to_string(),
                "include_archive": stored.include_archive,
                "archive_min_age_minutes": stored.archive_min_age_minutes,
                "profiles": profiles,
                "hint": "higher profiles widen candidate pools, walk further in the graph, and admit weaker matches",
            }))?;
            Ok(())
        }
    }
}

#[derive(Subcommand, Debug)]
enum GraphCommand {
    /// Plain-language explanation of a graph node and its neighbours
    /// (`brain explain` is Brain's own: why a decision was taken).
    Explain {
        node: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        rest: Vec<String>,
    },
}

/// Graphify keeps its own graph; Brain only forwards these commands so agents
/// have one `brain` command. Arguments go to the process directly, never
/// through a shell. `GRAPHIFY_GRAPH_JSON` selects the graph when it is set.
fn run_graphify(
    command: &str,
    positional: &[String],
    rest: &[String],
) -> Result<(), Box<dyn Error>> {
    let mut process = std::process::Command::new("graphify");
    process.arg(command).args(positional);
    let explicit_graph = rest
        .iter()
        .any(|arg| arg == "--graph" || arg.starts_with("--graph="));
    if let Some(graph) = std::env::var_os("GRAPHIFY_GRAPH_JSON")
        && !explicit_graph
    {
        process.arg("--graph").arg(graph);
    }
    process.args(rest);
    let status = process.status().map_err(|error| {
        format!("graphify could not be started ({error}); install it or put it on PATH")
    })?;
    if status.success() {
        return Ok(());
    }
    // Forward graphify's own status so scripts can tell its failures apart;
    // a child killed by a signal exits as a shell would report it (128 + signal).
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1);
    eprintln!("graphify {command} exited with {status}");
    std::process::exit(code);
}

fn command_status(config: &RuntimeConfig) -> Result<(), Box<dyn Error>> {
    let now = now_unix_seconds();
    let storage = open_storage(&config.database)?;
    storage.flush()?;
    let status = storage.status(&config.space, AsOf::new(now, now))?;
    let main_bytes = file_size(&config.database)?;
    let all_files_bytes = main_bytes
        .saturating_add(file_size(&suffix_path(&config.database, "-wal"))?)
        .saturating_add(file_size(&suffix_path(&config.database, "-shm"))?);
    let freshness_seconds = status
        .latest_recorded_at
        .map(|latest| now.saturating_sub(latest).max(0));
    print_json(&json!({
        "status": "ok",
        "database": config.database,
        "space": config.space,
        "schema_version": storage.schema_version()?,
        "nodes": status.nodes,
        "active_nodes": status.active_nodes,
        "edges": status.edges,
        "active_edges": status.active_edges,
        // Zero here with edges present means the graph walk has nothing to walk.
        // A database imported from another store arrived exactly that way, and
        // without this number nobody noticed for weeks.
        "graph_paths": storage.materialized_path_count(&config.space)?,
        "database_bytes": main_bytes,
        "database_files_bytes": all_files_bytes,
        "latest_recorded_at": status.latest_recorded_at,
        "freshness_seconds": freshness_seconds,
        "salience_layers": {
            "hot": status.hot_nodes,
            "warm": status.warm_nodes,
            "cold": status.cold_nodes,
        },
        "embeddings": status.embeddings,
    }))?;
    Ok(())
}

fn command_memory(config: &RuntimeConfig, command: MemoryCommand) -> Result<(), Box<dyn Error>> {
    let mut now = now_unix_seconds();
    let storage = open_storage(&config.database)?;
    match command {
        MemoryCommand::List { limit } => {
            let records = owned_memories(&storage, config, now, limit)?;
            let values = records.iter().map(memory_json).collect::<Vec<_>>();
            print_json(&json!({
                "status": "ok",
                "found": values.len(),
                "records": values,
            }))?;
        }
        MemoryCommand::Show { id } => {
            let record = find_owned_memory(&storage, config, now, &id)?;
            print_json(&json!({
                "status": "ok",
                "record": memory_json(&record),
            }))?;
        }
        MemoryCommand::Delete { id } => {
            let record = find_owned_memory(&storage, config, now, &id)?;
            while now <= record.node.valid_from || now <= record.node.recorded_at {
                thread::sleep(Duration::from_millis(10));
                now = now_unix_seconds();
            }
            let attribution = ensure_context(&storage, config, now)?;
            storage.forget_memory(
                &config.space,
                &id,
                &config.agent,
                &attribution.session_id,
                now,
                now,
            )?;
            print_json(&json!({
                "status": "forgotten",
                "id": id,
                "valid_to": now,
                "history_preserved": true,
            }))?;
        }
        MemoryCommand::Export { output } => {
            let records = owned_memories(&storage, config, now, MAX_MEMORY_SCAN)?;
            let payload = json!({
                "schema": "brain-memory-export-v1",
                "space": config.space,
                "agent": config.agent,
                "exported_at": now,
                "records": records.iter().map(memory_json).collect::<Vec<_>>(),
            });
            if let Some(path) = output {
                ensure_parent(&path)?;
                fs::write(&path, serde_json::to_vec_pretty(&payload)?)?;
                print_json(&json!({
                    "status": "exported",
                    "path": path,
                    "records": records.len(),
                }))?;
            } else {
                print_json(&payload)?;
            }
        }
    }
    Ok(())
}

fn owned_memories(
    storage: &SqliteStorage,
    config: &RuntimeConfig,
    now: Timestamp,
    limit: usize,
) -> Result<Vec<MemoryRecord>, Box<dyn Error>> {
    storage
        .list_memories(
            &config.space,
            Some(&config.agent),
            AsOf::new(now, now),
            limit,
        )?
        .into_iter()
        .map(MemoryRecord::from_node)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn find_owned_memory(
    storage: &SqliteStorage,
    config: &RuntimeConfig,
    now: Timestamp,
    id: &str,
) -> Result<MemoryRecord, Box<dyn Error>> {
    owned_memories(storage, config, now, MAX_MEMORY_SCAN)?
        .into_iter()
        .find(|record| record.node.id == id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "active memory {id:?} owned by agent {:?} was not found in space {:?}",
                    config.agent, config.space
                ),
            )
            .into()
        })
}

fn memory_json(record: &MemoryRecord) -> Value {
    json!({
        "id": record.node.id,
        "space": record.node.space_id,
        "kind": record.kind.to_string(),
        "text": record.node.body,
        "label": record.node.label,
        "tags": record.tags,
        "entities": record.entities,
        "source": record.node.source,
        "source_agent": record.node.agent_id,
        "source_session": record.node.session_id,
        "valid_from": record.node.valid_from,
        "valid_to": record.node.valid_to,
        "recorded_at": record.node.recorded_at,
        "salience": record.node.salience,
        "salience_layer": record.node.salience_layer.to_string(),
        "source_weight": record.node.source_weight,
        "token_len": record.node.token_len,
    })
}

pub(crate) fn ensure_context(
    storage: &SqliteStorage,
    config: &RuntimeConfig,
    now: Timestamp,
) -> Result<Attribution, Box<dyn Error>> {
    let session_id = format!("brain-cli:{}:{}", config.agent, now_unix_nanos());
    storage.ensure_context(
        &Agent {
            id: config.agent.clone(),
            display_name: Some(config.agent.clone()),
            metadata_json: r#"{"client":"brain-cli"}"#.to_owned(),
            created_at: now,
        },
        &Space {
            id: config.space.clone(),
            kind: SpaceKind::Project,
            label: config.space.clone(),
            metadata_json: r#"{"client":"brain-cli"}"#.to_owned(),
            created_by_agent_id: config.agent.clone(),
            created_at: now,
        },
        &Session {
            id: session_id.clone(),
            space_id: config.space.clone(),
            agent_id: config.agent.clone(),
            started_at: now,
            ended_at: None,
            metadata_json: r#"{"client":"brain-cli"}"#.to_owned(),
        },
    )?;
    Ok(Attribution {
        agent_id: config.agent.clone(),
        session_id,
        recorded_at: now,
    })
}

/// Whether the words alone answered well enough to skip the model.
///
/// "Enough" is a filled answer, not merely a non-empty one: a single weak hit
/// is exactly the case where meaning-based search earns its 2.5 seconds. A hit
/// the query matched verbatim counts for more than one the graph reached by
/// association.
fn lexical_answer_is_enough(
    output: &brain_core::SearchOutput,
    quality: &brain_core::profile::QualitySettings,
) -> bool {
    if output.hits.is_empty() {
        return false;
    }
    let direct = output
        .hits
        .iter()
        .filter(|hit| hit.matched_exact || hit.matched_bm25)
        .count();
    // Two thirds of the budget spent on real matches means the caller already
    // has an answer to read; a third of it means they have a fragment.
    direct >= 3 && output.used_tokens * 3 >= quality.budget_tokens * 2
}

fn load_cached_embedder(text: &str, config: &RuntimeConfig) -> EmbedderLoad {
    let home = brain_home();
    let plan = embedding_runtime::SemanticPlan::current(&home, text, &config.models_dir);
    if !plan.may_load_model() {
        return EmbedderLoad {
            #[cfg(feature = "model2vec")]
            embedder: None,
            warning: plan.warning(),
        };
    }
    #[cfg(feature = "model2vec")]
    {
        match Model2VecEmbedder::from_pretrained_cached(plan.model_id(), &config.models_dir, true) {
            Ok(embedder) => {
                let pending = embedding_runtime::pending_vector_jobs(&home, &config.database);
                let warning = if plan.drains_backlog_on_search() && pending > 0 {
                    match background::spawn_worker() {
                        Ok(()) => Some(format!(
                            "{pending} queued vector job(s) are being indexed in the background; \
                             this search may use BM25 + exact matches for them"
                        )),
                        Err(error) => Some(format!(
                            "{pending} vector job(s) are queued, but the background worker could \
                             not start: {error}"
                        )),
                    }
                } else {
                    None
                };
                EmbedderLoad {
                    embedder: Some(embedder),
                    warning,
                }
            }
            Err(error) => EmbedderLoad {
                embedder: None,
                warning: Some(format!(
                    "local semantic model {} is present but could not be loaded ({error}); using \
                     BM25 + exact matches only. Reinstall it with `{}`",
                    plan.model_id(),
                    background::install_command(plan.model_id()),
                )),
            },
        }
    }
    #[cfg(not(feature = "model2vec"))]
    {
        let _ = (&config.models_dir, config.offline, plan);
        EmbedderLoad {
            warning: plan.warning(),
        }
    }
}

/// The hidden worker re-checks the persisted mode in its own process.
///
/// This is intentionally before `run_worker`: that function creates a lock and
/// renames queue files, while `off` promises not to touch the backlog at all.
fn command_worker(home: &Path) -> Result<Value, Box<dyn Error>> {
    let mode = embedding_runtime::stored_mode(home);
    if mode == settings::EmbeddingMode::Off {
        return Ok(json!({
            "status": "disabled",
            "embedding_mode": "off",
            "processed": 0,
            // The worker serves every database, so its own report counts them all.
            "remaining": background::total_pending_embedding_jobs(home),
            "reason": "embedding mode is off; queued records were left untouched",
        }));
    }
    background::run_worker(home)
}

fn open_storage(path: &Path) -> Result<SqliteStorage, Box<dyn Error>> {
    ensure_parent(path)?;
    // Failures are explained once, at the exit point, so the path a message
    // takes through the code cannot double the advice the user reads.
    SqliteStorage::open(path)
        .map_err(|error| Box::<dyn Error>::from(format!("{}: {error}", path.display())))
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        // Brain's own home is made owner-only; an explicit external parent
        // only has its missing components created that way.
        let home = brain_home();
        if parent.starts_with(&home) {
            private_fs::ensure_private_home(&home)?;
        }
        private_fs::create_private_dir_all(parent)?;
    }
    Ok(())
}

fn resolve_database(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("BRAIN_DATABASE").map(PathBuf::from))
        .unwrap_or_else(|| brain_home().join("brain.sqlite"))
}

fn resolve_models_dir(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("BRAIN_MODELS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| brain_home().join("models"))
}

pub(crate) fn brain_home() -> PathBuf {
    user_home().join(".brain")
}

pub(crate) fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn stable_brain_binary() -> Result<PathBuf, Box<dyn Error>> {
    let executable = std::env::current_exe()?;
    let file_name = executable.file_name().and_then(|name| name.to_str());
    if matches!(file_name, Some("brain" | "brain.exe")) {
        Ok(executable)
    } else {
        Err(format!(
            "current executable path is versioned or unstable ({}); install a versionless `brain` binary before connecting agents",
            executable.display()
        )
        .into())
    }
}

fn default_agent() -> String {
    std::env::var("BRAIN_AGENT")
        .ok()
        .filter(|agent| !agent.trim().is_empty())
        .unwrap_or_else(|| "brain-cli".to_owned())
}

fn validate_scope(space: &str, agent: &str) -> io::Result<()> {
    if space.trim().is_empty() || agent.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "space and agent cannot be empty",
        ));
    }
    Ok(())
}

pub(crate) fn now_unix_seconds() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

fn now_unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn file_size(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

fn emit_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
}

fn print_json(value: &Value) -> Result<(), Box<dyn Error>> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn repeated_excerpts_keep_only_their_best_ranked_copy() {
        let hit = |role: &str, text: &str, rank: u32| json!({"client": "claude", "role": role, "excerpt": text, "rank": rank});
        let kept = super::distinct_excerpts(vec![
            hit("reasoning", "same thought", 0),
            hit("reasoning", "same thought", 1),
            hit("assistant", "same thought", 2),
            hit("reasoning", "other thought", 3),
        ]);
        let ranks: Vec<_> = kept.iter().map(|h| h["rank"].as_u64().unwrap()).collect();
        assert_eq!(ranks, [0, 2, 3]);
    }

    #[test]
    fn the_search_quoting_itself_is_filtered_in_every_form() {
        let old = "2026-08-01T00:00:00Z";
        let hits = [
            r#"for q in \"x\"; do $B ask \"$q\"; done; brain ask \"8803\""#,
            r#"{"archive_hits": 3, "archive_included": true}"#,
            "ran brain recall today",
            "the brain viewer runs on port 8765",
        ]
        .iter()
        .map(|text| json!({"excerpt": text, "timestamp": old}))
        .collect::<Vec<_>>();
        let (kept, recent, self_reference) = super::filter_archive_hits(hits, 120, 1_900_000_000);
        assert_eq!((recent, self_reference), (0, 3));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["excerpt"], "the brain viewer runs on port 8765");
    }

    #[test]
    fn tool_dumps_rank_after_conversation_errors_and_reasoning_in_stable_order() {
        let hits = [
            "tool_call",
            "assistant",
            "tool_result",
            "tool_error",
            "user",
            "reasoning",
        ]
        .iter()
        .enumerate()
        .map(|(rank, role)| json!({"role": role, "rank": rank}))
        .collect::<Vec<_>>();
        let order: Vec<_> = super::conversation_first(hits)
            .iter()
            .map(|hit| hit["role"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            order,
            [
                "assistant",
                "tool_error",
                "user",
                "reasoning",
                "tool_call",
                "tool_result"
            ]
        );
    }

    use super::*;
    use clap::CommandFactory;

    #[test]
    fn e65_settings_commands_parse_and_limit_is_bounded() {
        assert!(Cli::try_parse_from(["brain", "settings", "history"]).is_ok());
        assert!(Cli::try_parse_from(["brain", "settings", "history", "--limit", "10"]).is_ok());
        for limit in ["0", "10001", "-1", "bad"] {
            assert!(
                Cli::try_parse_from(["brain", "settings", "history", "--limit", limit]).is_err()
            );
        }
        assert!(
            Cli::try_parse_from(["brain", "settings", "revert", "--to", "123", "--yes"]).is_ok()
        );
    }

    #[test]
    fn parses_required_a5_commands() {
        for arguments in [
            vec!["brain", "remember", "SQLite stores memory"],
            vec!["brain", "ask", "SQLite"],
            vec!["brain", "recall", "SQLite", "--budget-tokens", "50"],
            vec!["brain", "status"],
            vec!["brain", "memory", "list"],
            vec!["brain", "memory", "show", "mem_1"],
            vec!["brain", "memory", "delete", "mem_1"],
            vec!["brain", "memory", "export"],
            vec!["brain", "serve"],
            vec!["brain", "brief"],
            vec![
                "brain",
                "hook",
                "--event",
                "SessionStart",
                "--client",
                "codex",
            ],
            vec!["brain", "init", "--agents", "--dry-run"],
            vec!["brain", "uninstall", "--dry-run"],
        ] {
            Cli::try_parse_from(arguments).expect("A5 command should parse");
        }
    }

    #[test]
    fn positional_init_path_remains_supported() {
        let cli = Cli::try_parse_from(["brain", "init", "custom.sqlite"]).expect("parse init path");
        let Command::Init { path, .. } = cli.command else {
            panic!("expected init command");
        };
        assert_eq!(path, Some(PathBuf::from("custom.sqlite")));
    }

    #[test]
    fn remember_help_and_parser_expose_error_kind() {
        assert_eq!("error".parse::<MemoryKind>().unwrap(), MemoryKind::Error);
        let help = Cli::command()
            .find_subcommand_mut("remember")
            .expect("remember subcommand")
            .render_long_help()
            .to_string();
        assert!(help.contains("gotcha, error, or note"));
    }

    /// Every command and subcommand in the tree carries a description (G2):
    /// clap's built-in `help` subcommand is the only exempt one.
    #[test]
    fn every_subcommand_has_a_description() {
        fn check(command: &clap::Command, path: &str, problems: &mut Vec<String>) {
            let name = if path.is_empty() {
                command.get_name().to_string()
            } else {
                format!("{path} {}", command.get_name())
            };
            if command
                .get_about()
                .map(|about| about.to_string().trim().is_empty())
                .unwrap_or(true)
                && command.get_name() != "help"
                && !path.is_empty()
            {
                problems.push(name.clone());
            }
            for sub in command.get_subcommands() {
                if sub.get_name() == "help" {
                    continue;
                }
                check(sub, &name, problems);
            }
        }
        let mut problems = Vec::new();
        check(&Cli::command(), "", &mut problems);
        assert!(
            problems.is_empty(),
            "subcommands without a description: {problems:?}"
        );
    }

    /// Every option (flag) of every command carries help text (G2).
    #[test]
    fn every_option_has_help() {
        fn check(command: &clap::Command, path: &str, problems: &mut Vec<String>) {
            let name = if path.is_empty() {
                command.get_name().to_string()
            } else {
                format!("{path} {}", command.get_name())
            };
            for argument in command.get_arguments() {
                if argument.is_positional() || argument.get_long() == Some("help") {
                    continue;
                }
                let documented = argument
                    .get_help()
                    .is_some_and(|help| !help.to_string().trim().is_empty())
                    || argument
                        .get_long_help()
                        .is_some_and(|help| !help.to_string().trim().is_empty());
                if !documented {
                    problems.push(format!("{name} --{}", argument.get_long().unwrap_or("?")));
                }
            }
            for sub in command.get_subcommands() {
                if sub.get_name() == "help" {
                    continue;
                }
                check(sub, &name, problems);
            }
        }
        let mut problems = Vec::new();
        check(&Cli::command(), "", &mut problems);
        assert!(
            problems.is_empty(),
            "options without help text: {problems:?}"
        );
    }
}
