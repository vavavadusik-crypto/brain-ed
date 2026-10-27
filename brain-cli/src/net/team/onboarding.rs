//! Host-local onboarding. All secret-bearing files are bounded, owner-only,
//! opened without symlinks; no secret is included in command output.
use super::super::ConnectArgs;
use super::*;
use brain_core::network::{AuthStore, TlsCallResult, read_credential};
use clap::Args;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Debug, Args)]
pub(crate) struct Invite {
    #[command(flatten)]
    local: Local,
    /// Team role: worker publishes its slice; manager reads selected machines.
    #[arg(long, value_parser=["worker","manager"])]
    role: String,
    /// Unique machine name, 3..64 lowercase letters, digits and hyphens.
    #[arg(long)]
    name: String,
    /// Project id, used as an explicit team.PROJECT shared-space boundary.
    #[arg(long, default_value = "default")]
    project: String,
    /// Invitation lifetime: seconds or suffix s, m, h, d (60s..90d).
    #[arg(long, default_value = "7d")]
    expires: String,
    /// New invitation file, 0600. Default: NAME.invite.json in current directory.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Coordinator address; default: this Brain home's running net serve.
    #[arg(long)]
    dial: Option<String>,
    /// Coordinator certificate SAN; default: first name from net init.
    #[arg(long)]
    server_name: Option<String>,
    /// Permit this worker to register a private LAN source endpoint.
    #[arg(long)]
    allow_lan_source: bool,
    /// Permit this worker to register a CGNAT source endpoint.
    #[arg(long)]
    allow_cgnat_source: bool,
}
#[derive(Debug, Args)]
pub(crate) struct Accept {
    file: PathBuf,
    #[command(flatten)]
    local: Local,
    /// Explicitly approve collection, enable the local slice and publish now.
    #[arg(long)]
    publish: bool,
    /// Worker project root (default: current directory); never sent to coordinator.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Local source space to project; no archive access is granted.
    #[arg(long, default_value = "default")]
    source_space: String,
    /// Worker's already running net serve endpoint; default: local runtime observation.
    #[arg(long)]
    source_dial: Option<String>,
    /// Explicitly allow dialing the coordinator on a private LAN address.
    #[arg(long)]
    allow_lan_dial: bool,
    /// Explicitly allow dialing the coordinator on a CGNAT address.
    #[arg(long)]
    allow_cgnat_dial: bool,
    /// Approve publishing a private LAN source endpoint (also requires admin opt-in).
    #[arg(long)]
    allow_lan_source: bool,
    /// Approve publishing a CGNAT source endpoint (also requires admin opt-in).
    #[arg(long)]
    allow_cgnat_source: bool,
}
#[derive(Debug, Args)]
pub(crate) struct Remove {
    name: String,
    #[command(flatten)]
    local: Local,
}
fn new_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}
fn private_json(path: &Path) -> Result<Value> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file()
        || m.len() > 65536
        || m.mode() & 0o077 != 0
        || Some(m.uid()) != crate::private_fs::current_uid()
    {
        return Err("team file must be an owner-only regular file, at most 64 KiB".into());
    }
    let mut bytes = vec![];
    f.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err("team file too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid team file".into())
}
fn runtime_dial(state: &Path) -> Result<String> {
    let observation = private_json(&super::super::window_observation_path(&crate::brain_home()))?;
    if observation["state_dir"].as_str() != std::fs::canonicalize(state)?.to_str()
        || observation["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .and_then(super::super::process_start)
            .as_deref()
            != observation["started"].as_str()
    {
        return Err(
            "start brain net serve for this --state-dir or pass an explicit dial address".into(),
        );
    }
    Ok(observation["bind"]
        .as_str()
        .ok_or("missing listener address")?
        .into())
}
fn duration(s: &str) -> Result<i64> {
    let (digits, mult) = match s.as_bytes().last() {
        Some(b's') => (&s[..s.len() - 1], 1),
        Some(b'm') => (&s[..s.len() - 1], 60),
        Some(b'h') => (&s[..s.len() - 1], 3600),
        Some(b'd') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    let n = digits
        .parse::<i64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| (60..=90 * 86400).contains(n))
        .ok_or("--expires must be 60s..90d")?;
    Ok(n)
}
pub(super) fn invite(args: Invite) -> Result<Value> {
    let state = dir(&args.local);
    if state.join("membership.json").exists() {
        return Err("invite is local coordinator admin only".into());
    }
    let ttl = duration(&args.expires)?;
    let dial = args.dial.map(Ok).unwrap_or_else(|| runtime_dial(&state))?;
    // Invitation data may contain LAN addresses; accept still requires its own dial opt-in.
    brain_core::network::parse_dial_target_with(&dial, true, true)
        .map_err(|_| "invalid coordinator address")?;
    let ca = std::fs::read_to_string(state.join("tls/ca.pem"))?;
    let server_name = args
        .server_name
        .or_else(|| crate::bundle::first_dns_name(&state))
        .ok_or("run net init --name first")?;
    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("{}.invite.json", args.name)));
    if out.symlink_metadata().is_ok() {
        return Err("invitation output already exists".into());
    }
    let auth = AuthStore::open(&state)?;
    let token = auth.team_invite(&args.name, &args.role, &args.project, ttl)?;
    auth.team_route_policy(&args.name, args.allow_lan_source, args.allow_cgnat_source)?;
    let document = json!({"schema":"brain-team-invite/1","role":args.role,"project":args.project,
        "connection":{"version":1,"server_name":server_name,"dial":[dial],"ca_pem":ca,
            "credential":token.as_str(),"machine":args.name,"agent_id":"team","expires_at":super::super::unix_now()+ttl}});
    if new_secret(&out, &serde_json::to_vec_pretty(&document)?).is_err() {
        let _ = auth.disable_principal(&args.name);
        return Err("cannot write invitation; account revoked".into());
    }
    Ok(
        json!({"status":"ok","name":args.name,"role":args.role,"project":args.project,"invitation":out,"expires_at":document["connection"]["expires_at"]}),
    )
}
fn connect(client: &Path) -> Result<ConnectArgs> {
    let client = std::fs::canonicalize(client)?;
    let ep = crate::bundle::load_endpoint(&client).ok_or("missing joined endpoint")?;
    serde_json::from_value(json!({"dial":ep["dial"][0],"server_name":ep["server_name"],
        "ca":client.join("ca.pem"),"credential":client.join("credential.pem"),"credential_fd":null,
        "allow_lan_dial":ep["allow_lan_dial"],"allow_cgnat_dial":ep["allow_cgnat_dial"],"deadline_ms":5000})).map_err(|_|"invalid joined endpoint".into())
}
fn call(connect: &ConnectArgs, tool: &str, args: Value) -> Result<Value> {
    let (client, _) =
        super::super::client_explicit(connect).map_err(|_| "invalid team connection")?;
    match client.call(
        tool,
        "",
        args.as_object().ok_or("invalid arguments")?.clone(),
        None,
    ) {
        TlsCallResult::Ok(body) => Ok(body["result"].clone()),
        TlsCallResult::Refused(_) => Err(
            "team request denied (invitation used, expired, revoked, or wrong role/project)".into(),
        ),
        _ => Err("team coordinator unavailable; no cached answer is served".into()),
    }
}
pub(super) fn accept(config: &RuntimeConfig, args: Accept) -> Result<Value> {
    let document = private_json(&args.file)?;
    if document["schema"] != "brain-team-invite/1" {
        return Err("unsupported invitation".into());
    }
    let role = document["role"].as_str().ok_or("missing role")?;
    let project = document["project"]
        .as_str()
        .filter(|s| team::id(s) && s.len() <= 48)
        .ok_or("invalid project")?;
    if !["manager", "worker"].contains(&role) || (role == "manager" && args.publish) {
        return Err("manager cannot publish".into());
    }
    let state = dir(&args.local);
    crate::private_fs::ensure_private_home(&state)?;
    let client_dir = state.join("client");
    if client_dir.symlink_metadata().is_ok()
        || state.join("membership.json").symlink_metadata().is_ok()
    {
        return Err("already registered; use a separate Brain home".into());
    }
    let name = document["connection"]["machine"]
        .as_str()
        .ok_or("missing name")?
        .to_owned();
    let space = format!("team.{project}");
    let root = std::fs::canonicalize(args.root.unwrap_or(std::env::current_dir()?))?;
    if !root.is_dir() {
        return Err("project root must be a directory".into());
    }
    let mut publish = args.publish;
    if role == "worker" && !publish && std::io::stdin().is_terminal() {
        eprint!("Publish the bounded team slice for project {project}? [y/N] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        publish = matches!(
            answer.trim().to_lowercase().as_str(),
            "y" | "yes" | "д" | "да"
        );
    }
    let staging = state.join(format!(".team-accept-{}", std::process::id()));
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let mut issued_source_token: Option<String> = None;
    let result = (|| -> Result<Value> {
        let bundle = staging.join("join.json");
        new_secret(&bundle, &serde_json::to_vec(&document["connection"])?)?;
        let staged_client = staging.join("client");
        crate::bundle::join(
            &bundle,
            &staged_client,
            args.allow_lan_dial,
            args.allow_cgnat_dial,
            true,
            false,
        )?;
        let mut source = None;
        let requested_source_dial = args.source_dial.clone();
        if role == "worker" {
            crate::bundle::init(&state, &["localhost".into()], &[], false)?;
            let source_dial = args
                .source_dial
                .map(Ok)
                .unwrap_or_else(|| runtime_dial(&state));
            if publish && source_dial.is_err() {
                return Err(
                    "--publish requires a running brain net serve for this state directory".into(),
                );
            }
            // A dedicated local reader is used only by the coordinator, never by managers.
            let auth = AuthStore::open(&state)?;
            let gateway = format!(
                "gw-{}",
                super::super::fresh_request_id()
                    .map_err(|_| "cannot create source reader identity")?
            );
            auth.upsert_principal(&gateway, "gateway", "Team coordinator", "reader", &[&space])?;
            auth.grant_memory_read(&gateway, "gateway", &space)?;
            let token = auth.issue_credential(&gateway, "gateway", None)?;
            let token_path = state.join("team-source.credential");
            if let Err(error) = new_secret(&token_path, token.as_str().as_bytes()) {
                let _ = auth.revoke_credential(token.token_id().as_str());
                return Err(error);
            }
            issued_source_token = Some(token.token_id().as_str().to_owned());
            execute(
                config,
                TeamCommand::Configure(Configure {
                    local: Local {
                        state_dir: Some(state.clone()),
                        team_space: Some(space.clone()),
                    },
                    machine: name.clone(),
                    project: project.into(),
                    root,
                    source_space: args.source_space,
                    recipients: vec![format!("{gateway}:gateway")],
                }),
            )?;
            if let Ok(dial) = source_dial {
                source = Some(
                    json!({"dial":dial,"server_name":crate::bundle::first_dns_name(&state).ok_or("source TLS name missing")?,
                    "ca_pem":std::fs::read_to_string(state.join("tls/ca.pem"))?,"credential":token.as_str(),
                    "origin":ShareStore::open_readonly(state.join(SHARE_STORE_FILE))?.install_id(),
                    "allow_lan":args.allow_lan_source,"allow_cgnat":args.allow_cgnat_source}),
                );
            }
        }
        let mut request = json!({"space":space,"expected_role":role,"expected_name":name});
        if let Some(source) = source {
            request["source"] = source;
        }
        let response = call(&connect(&staged_client)?, "team_accept", request)?;
        let credential = response["credential"]
            .as_str()
            .ok_or("invalid accept response")?;
        brain_core::network::BearerToken::parse(credential)?;
        crate::private_fs::write_private(
            &staged_client.join("credential.pem"),
            credential.as_bytes(),
        )?;
        let endpoint_path = crate::bundle::endpoint_path(&staged_client);
        let mut endpoint =
            crate::bundle::load_endpoint(&staged_client).ok_or("invalid endpoint")?;
        endpoint["expires_at"] = response["expires_at"].clone();
        crate::private_fs::write_private(&endpoint_path, &serde_json::to_vec(&endpoint)?)?;
        std::fs::rename(&staged_client, &client_dir)?;
        new_secret(
            &state.join("membership.json"),
            &serde_json::to_vec(&json!({"name":name,"role":role,"project":project,"space":space}))?,
        )?;
        if role == "worker" {
            new_secret(
                &state.join("source-route.json"),
                &serde_json::to_vec(
                    &json!({"dial":requested_source_dial,"allow_lan":args.allow_lan_source,"allow_cgnat":args.allow_cgnat_source}),
                )?,
            )?;
        }
        if role == "manager" {
            sync(&state)?;
        }
        if publish {
            toggle_at(&state, &space, None, true)?;
            execute(
                config,
                TeamCommand::Publish(Publish {
                    local: Local {
                        state_dir: Some(state.clone()),
                        team_space: Some(space.clone()),
                    },
                    scheduled: false,
                }),
            )?;
        }
        Ok(
            json!({"status":"ok","name":name,"role":role,"project":project,"publishing":publish,
            "next":if role=="worker"&&!publish {"publication is OFF; start net serve, then explicitly team enable and team publish"}else{"team who / team ask"}}),
        )
    })();
    let _ = std::fs::remove_dir_all(staging);
    if result.is_err()
        && !client_dir.exists()
        && let Some(token_id) = issued_source_token
    {
        if let Ok(auth) = AuthStore::open(&state) {
            let _ = auth.revoke_credential(&token_id);
        }
        let _ = std::fs::remove_file(state.join("team-source.credential"));
        // Configuration written before redemption stays OFF; a retry can safely reconfigure it.
    }
    result
}
pub(super) fn sync(state: &Path) -> Result<Value> {
    let connection = connect(&state.join("client"))?;
    let directory = call(&connection, "team_directory", json!({}))?;
    let store = ShareStore::open(state.join(SHARE_STORE_FILE))?;
    for p in directory["peers"]
        .as_array()
        .ok_or("invalid team directory")?
    {
        let name = p["machine"]
            .as_str()
            .filter(|s| team::id(s))
            .ok_or("invalid team machine")?;
        store.team_save_peer(name,&json!({"schema":"brain-team-peer/1","machine":name,"origin":p["origin"],"project":p["project"],"space":p["space"],"connect":connection,"via_coordinator":true}))?;
    }
    Ok(directory["team"].clone())
}
pub(super) fn who(local: Local) -> Result<Value> {
    let state = dir(&local);
    if state.join("membership.json").exists() {
        return sync(&state);
    }
    if !state.join("auth.sqlite").exists() {
        return Err("no coordinator; use team accept FILE".into());
    }
    Ok(AuthStore::open(&state)?.team_who(None, super::super::unix_now())?)
}
pub(super) fn remove(args: Remove) -> Result<Value> {
    let state = dir(&args.local);
    if state.join("membership.json").exists() {
        return Err("remove is local coordinator admin only".into());
    }
    if !state.join("auth.sqlite").exists() {
        return Err("no coordinator auth store".into());
    }
    AuthStore::open(&state)?.disable_principal(&args.name)?;
    Ok(
        json!({"status":"ok","removed":args.name,"effective":"next request; previously received data cannot be erased"}),
    )
}
pub(super) fn role(state: &Path) -> Option<String> {
    private_json(&state.join("membership.json"))
        .ok()?
        .get("role")?
        .as_str()
        .map(str::to_owned)
}
pub(super) fn refresh_if_joined(state: &Path) -> ShareResult<()> {
    if role(state).as_deref() == Some("manager") {
        sync(state).map_err(|_| team::bad())?;
    }
    Ok(())
}

/// Re-register current endpoint before each explicit publication, so accept
/// without a running server can remain OFF until the owner opts in later.
pub(super) fn register_source(state: &Path) -> ShareResult<()> {
    if role(state).as_deref() != Some("worker") {
        return Ok(());
    }
    (||->Result<()> {
        let member=private_json(&state.join("membership.json"))?;
        let ep=private_json(&state.join("source-route.json"))?;
        let dial=ep["dial"].as_str().map(str::to_owned).map(Ok).unwrap_or_else(||runtime_dial(state))?;
        let source=json!({"dial":dial,"server_name":crate::bundle::first_dns_name(state).ok_or("source TLS name missing")?,
            "ca_pem":std::fs::read_to_string(state.join("tls/ca.pem"))?,
            "credential":read_credential(&state.join("team-source.credential"))?,
            "origin":ShareStore::open_readonly(state.join(SHARE_STORE_FILE))?.install_id(),
            "allow_lan":ep["allow_lan"],"allow_cgnat":ep["allow_cgnat"]});
        call(&connect(&state.join("client"))?,"team_register",json!({"space":member["space"],"source":source}))?;
        Ok(())
    })().map_err(|_|team::bad())
}
