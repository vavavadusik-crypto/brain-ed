//! Explicit host-local opt-in and verified replica queries. No source archives are opened here.
use crate::RuntimeConfig;
use brain_core::network::SHARE_STORE_FILE;
use brain_core::sharing::{ShareResult, ShareStore};
use brain_core::team::{self, CLASSES, Manifest, Recipient};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
mod intent;
pub(crate) mod mcp;
mod project;
mod query;
mod schedule;

#[derive(Debug, Subcommand)]
pub(crate) enum TeamCommand {
    /// Coordinator admin: pin a source machine, origin, project, space and TLS endpoint.
    Peer(query::PeerArgs),
    /// Search only complete verified replicas of explicitly selected machines.
    Ask(query::AskArgs),
    /// Read-only local manifest: what, to whom and when. Missing policy means off.
    Manifest(Local),
    /// Read-only configured machine names, projects and last observed slice freshness.
    Peers(Local),
    /// Host admin: bind project root, machine name, isolated space and recipients. Starts OFF.
    Configure(Configure),
    /// Enable this machine (or one class); a new publish is required before serving.
    Enable(Toggle),
    /// Local opt-out overrides every remote grant immediately.
    Disable(Toggle),
    /// Project allowed local sources and atomically publish a complete share generation.
    Publish(Publish),
    /// Opt-in user timer for bounded publication.
    Schedule {
        #[command(subcommand)]
        command: schedule::ScheduleCommand,
    },
}
#[derive(Debug, Args)]
pub(crate) struct Publish {
    #[command(flatten)]
    local: Local,
    /// Timer invocation: respect local opt-out and persist a bounded status.
    #[arg(long, hide = true)]
    scheduled: bool,
}
#[derive(Debug, Default, Args)]
pub(crate) struct Local {
    /// Defaults to ~/.brain/net. Explicit --state-dir is recommended for isolated use.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Isolated team space, e.g. team.project.readers.
    #[arg(long)]
    team_space: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct Toggle {
    #[command(flatten)]
    local: Local,
    /// One fact class to switch instead of the whole machine.
    #[arg(long="class",value_parser=CLASSES.to_vec())]
    class: Option<String>,
    /// Disable one allowlisted content field. Required fields cause record refusal.
    #[arg(long, requires = "class")]
    field: Option<String>,
}
#[derive(Debug, Args)]
pub(crate) struct Configure {
    #[command(flatten)]
    local: Local,
    /// This machine's enrolled name, as the coordinator and manager see it.
    #[arg(long)]
    machine: String,
    /// Project id published with every fact.
    #[arg(long)]
    project: String,
    /// Project root; only paths inside it are considered, and none are exported.
    #[arg(long)]
    root: PathBuf,
    /// Local memory space the facts are projected from.
    #[arg(long, default_value = "default")]
    source_space: String,
    /// Exact enrolled recipient machine:agent pair (repeatable).
    #[arg(long = "recipient", required = true)]
    recipients: Vec<String>,
}
fn dir(local: &Local) -> PathBuf {
    local
        .state_dir
        .clone()
        .unwrap_or_else(|| crate::brain_home().join("net"))
}
fn selected(store: &ShareStore, local: &Local) -> ShareResult<String> {
    if let Some(space) = &local.team_space {
        if !team::is_team(space) || !team::id(space) {
            return Err(team::bad());
        }
        return Ok(space.clone());
    }
    let all = store.team_manifests()?;
    if all.len() != 1 {
        return Err(team::bad());
    }
    Ok(all[0].space.clone())
}
pub(crate) fn manifest_document(state: &Path) -> ShareResult<Value> {
    let path = state.join(SHARE_STORE_FILE);
    if !path.exists() {
        return Ok(json!({"status":"ok","enabled":false,"manifests":[],"classes":CLASSES}));
    }
    let store = ShareStore::open_readonly(path)?;
    let manifests = store.team_manifests()?;
    let mut out = vec![];
    for m in manifests {
        let slice = store.team_active_slice(&m.space)?;
        let ready = m.enabled
            && slice
                .as_ref()
                .is_some_and(|s| s.manifest_revision == m.revision);
        out.push(json!({"manifest":m,"ready":ready,"last_slice":slice}));
    }
    Ok(
        json!({"status":"ok","manifests":out,"classes":CLASSES,"fields":CLASSES.iter().map(|c|((*c).to_owned(),team::fields(c))).collect::<std::collections::BTreeMap<_,_>>(),"integrity":"tls+blake3-unsigned","schedule":"manual_or_user_timer"}),
    )
}
pub(crate) fn toggle_at(
    state: &Path,
    space: &str,
    class: Option<&str>,
    enabled: bool,
) -> ShareResult<Value> {
    let mut store = ShareStore::open(state.join(SHARE_STORE_FILE))?;
    let m = store.team_toggle(space, class, enabled, super::unix_now())?;
    Ok(json!({"status":"ok","manifest":m,"ready":false,"next":"publish"}))
}
pub(crate) fn toggle_field_at(
    state: &Path,
    space: &str,
    class: &str,
    field: &str,
    enabled: bool,
) -> ShareResult<Value> {
    let mut store = ShareStore::open(state.join(SHARE_STORE_FILE))?;
    let m = store.team_toggle_field(space, class, field, enabled, super::unix_now())?;
    Ok(json!({"status":"ok","manifest":m,"ready":false,"next":"publish"}))
}
fn execute(config: &RuntimeConfig, cmd: TeamCommand) -> ShareResult<Value> {
    match cmd {
        TeamCommand::Peer(args) => query::configure(args),
        TeamCommand::Ask(args) => query::ask(config, args),
        TeamCommand::Manifest(local) => manifest_document(&dir(&local)),
        TeamCommand::Peers(local) => query::peers_document(&dir(&local)),
        TeamCommand::Configure(args) => {
            let state = dir(&args.local);
            let space = args
                .local
                .team_space
                .as_ref()
                .ok_or_else(team::bad)?
                .clone();
            let root = std::fs::canonicalize(args.root).map_err(|_| team::bad())?;
            if !root.is_dir() {
                return Err(team::bad());
            }
            let recipients = args
                .recipients
                .iter()
                .map(|s| {
                    let (machine, agent) = s.split_once(':').ok_or_else(team::bad)?;
                    Ok(Recipient {
                        machine: machine.into(),
                        agent: agent.into(),
                    })
                })
                .collect::<ShareResult<Vec<_>>>()?;
            let mut store = ShareStore::open(state.join(SHARE_STORE_FILE))?;
            let manifest = Manifest {
                schema: "brain-team-manifest/1".into(),
                revision: 1,
                approved_at: super::unix_now(),
                enabled: false,
                origin: store.install_id().into(),
                machine_id: args.machine,
                project_id: args.project,
                project_root: root.to_str().ok_or_else(team::bad)?.into(),
                source_space: args.source_space,
                space: space.clone(),
                recipients,
                classes: CLASSES.iter().map(|s| (*s).into()).collect::<BTreeSet<_>>(),
                disabled_fields: BTreeSet::new(),
                annotation_policy: "automatic-masked".into(),
                integrity: "tls+blake3-unsigned".into(),
                snapshot_retention_days: 90,
                audit_retention_days: 365,
                stale_after_seconds: team::DAY,
            };
            manifest.validate()?;
            let m = store.team_configure(manifest)?;
            store.enable_space(&space)?;
            Ok(json!({"status":"ok","manifest":m,"ready":false}))
        }
        TeamCommand::Enable(args) | TeamCommand::Disable(args) => unreachable_toggle(args),
        TeamCommand::Schedule { .. } => Err(team::bad()),
        TeamCommand::Publish(args) => {
            let local = args.local;
            let mut store = ShareStore::open(dir(&local).join(SHARE_STORE_FILE))?;
            let space = selected(&store, &local)?;
            let m = store.team_manifest(&space)?.ok_or_else(team::bad)?;
            if !m.enabled {
                return Err(team::bad());
            }
            let start = super::unix_now();
            let (facts, watermarks, rejected) = project::collect(config, &m, start)?;
            store.team_publish(
                &space,
                facts,
                watermarks,
                rejected,
                start,
                super::unix_now(),
            )
        }
    }
}
fn unreachable_toggle(_: Toggle) -> ShareResult<Value> {
    Err(team::bad())
}
pub(crate) fn run(config: &RuntimeConfig, command: TeamCommand) -> Result<(), Box<dyn Error>> {
    if let TeamCommand::Schedule { command } = command {
        return crate::print_json(&schedule::command(config, command)?);
    }
    let result = match command {
        TeamCommand::Publish(args) if args.scheduled => schedule::publish(config, args),
        TeamCommand::Enable(args) => toggle_command(args, true),
        TeamCommand::Disable(args) => toggle_command(args, false),
        other => execute(config, other),
    };
    match result {
        Ok(value) => super::finish(super::Exit::Ok, &value),
        Err(_) => super::finish(
            super::Exit::Refused,
            &json!({"status":"denied","code":"team_validation_failed"}),
        ),
    }
}
fn toggle_command(args: Toggle, enabled: bool) -> ShareResult<Value> {
    let state = dir(&args.local);
    let store = ShareStore::open_readonly(state.join(SHARE_STORE_FILE))?;
    let space = selected(&store, &args.local)?;
    drop(store);
    if let Some(field) = args.field.as_deref() {
        return toggle_field_at(
            &state,
            &space,
            args.class.as_deref().ok_or_else(team::bad)?,
            field,
            enabled,
        );
    }
    toggle_at(&state, &space, args.class.as_deref(), enabled)
}
