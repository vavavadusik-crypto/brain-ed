use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::client_home::{ClientHome, ClientHomes};

const MD_BEGIN: &str = "<!-- BRAIN_FOR_AI_BEGIN -->";
const MD_END: &str = "<!-- BRAIN_FOR_AI_END -->";
const CONFIG_BEGIN: &str = "# BRAIN_FOR_AI_BEGIN";
const CONFIG_END: &str = "# BRAIN_FOR_AI_END";
const DESKTOP_COPY_OWNER: &str = "desktop-shortcut-owner";
/// Bounds for the files `brain uninstall` reads. The manifest Brain writes is
/// a few KiB; client configuration files (and so their backups) can be large.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_CLIENT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// `O_NOFOLLOW` where its value is known without a new dependency.
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NOFOLLOW: i32 = 0o400000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AgentClient {
    Claude,
    Codex,
    Cursor,
}

impl AgentClient {
    const fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Cursor => "Cursor",
        }
    }
}

#[derive(Debug)]
struct PlannedChange {
    client: AgentClient,
    path: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    description: String,
    conflict: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstallManifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    path: PathBuf,
    existed: bool,
    before_sha256: Option<String>,
    after_sha256: String,
    backup: Option<PathBuf>,
    mode: Option<u32>,
}

pub(crate) fn install(
    home: &Path,
    brain_home: &Path,
    binary: &Path,
    dry_run: bool,
    yes: bool,
) -> Result<usize, Box<dyn Error>> {
    // Before the client-home hints are classified: nothing runs on an
    // unusable HOME.
    crate::private_fs::require_safe_homes(home, brain_home)?;
    let homes = crate::client_home::from_process_env(home);
    install_with(home, brain_home, binary, dry_run, yes, &homes)
}

fn install_with(
    home: &Path,
    brain_home: &Path,
    binary: &Path,
    dry_run: bool,
    yes: bool,
    homes: &ClientHomes,
) -> Result<usize, Box<dyn Error>> {
    // First, dry runs included: an unset (`.`), empty, relative or
    // control-character HOME would plan client files, hooks and the stable
    // copy relative to the current directory or with unwritable bytes.
    crate::private_fs::require_safe_homes(home, brain_home)?;
    validate_versionless_binary(binary)?;
    let skipped = relocated_clients(homes);
    for (client, location) in &skipped {
        println!("{}", relocation_conflict(*client, *location, "install"));
    }
    let clients = detect_clients(home, homes);
    if clients.is_empty() {
        if skipped.is_empty() {
            println!("No supported agents detected; nothing to change.");
        } else {
            println!("No other supported agents detected; nothing to change.");
        }
        return Ok(0);
    }
    println!(
        "Detected agents: {}",
        clients
            .iter()
            .map(|client| client.name())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let hook_binary = stable_copy_path(brain_home);
    if !dry_run && install_stable_copy(binary, &hook_binary)? {
        println!("Installed the Brain binary at {}.", hook_binary.display());
    }
    let plans = build_plans(home, &hook_binary, &clients);
    let manifest_path = brain_home.join("install-manifest.json");
    let existing_manifest = manifest_path.exists();
    let actionable = plans
        .iter()
        .filter(|plan| plan.conflict.is_none() && plan.before != plan.after)
        .count();
    if existing_manifest && actionable > 0 && !dry_run {
        return Err("an installation manifest already exists and files drifted; run brain uninstall or use --dry-run to inspect".into());
    }

    for plan in &plans {
        println!("\n{}: {}", plan.client.name(), plan.description);
        if let Some(conflict) = &plan.conflict {
            println!("CONFLICT {}: {conflict}", plan.path.display());
        } else if plan.before == plan.after {
            println!("unchanged {}", plan.path.display());
        } else {
            print!(
                "{}",
                render_diff(&plan.path, plan.before.as_deref(), plan.after.as_deref())
            );
        }
    }
    if dry_run {
        println!("\nDry run: {actionable} file(s) would change; no files written.");
        return Ok(0);
    }
    if actionable == 0 {
        println!("Brain agent integration is already up to date.");
        return Ok(0);
    }

    let approved = plans
        .iter()
        .map(|plan| {
            if plan.conflict.is_some() || plan.before == plan.after {
                Ok(false)
            } else if yes {
                Ok(true)
            } else {
                confirm(&format!("Apply change to {}?", plan.path.display()))
            }
        })
        .collect::<io::Result<Vec<_>>>()?;

    let backup_dir = brain_home.join("install-backups");
    crate::private_fs::ensure_private_home(brain_home)?;
    crate::private_fs::ensure_private_home(&backup_dir)?;
    let selected = plans
        .iter()
        .zip(&approved)
        .enumerate()
        .filter_map(|(index, (plan, approved))| approved.then_some((index, plan)))
        .collect::<Vec<_>>();
    for (_, plan) in &selected {
        ensure_not_git_managed(&plan.path)?;
    }

    let mut prepared = Vec::new();
    for (index, plan) in &selected {
        let after = plan
            .after
            .as_deref()
            .ok_or("install plan unexpectedly removes a file")?;
        let backup = if let Some(before) = &plan.before {
            let relative = PathBuf::from(format!("{index:03}.bak"));
            crate::private_fs::write_private(&backup_dir.join(&relative), before)?;
            Some(relative)
        } else {
            None
        };
        let mode = file_mode(&plan.path);
        prepared.push((
            *plan,
            ManifestEntry {
                path: plan.path.clone(),
                existed: plan.before.is_some(),
                before_sha256: plan.before.as_deref().map(sha256),
                after_sha256: sha256(after),
                backup,
                mode,
            },
        ));
    }

    for (written, (plan, entry)) in prepared.iter().enumerate() {
        let after = plan
            .after
            .as_deref()
            .ok_or("install plan unexpectedly removes a file")?;
        if let Err(error) = atomic_write(&plan.path, after, entry.mode) {
            rollback_plans(&prepared[..written]);
            return Err(format!("failed to update {}: {error}", plan.path.display()).into());
        }
    }
    let applied = prepared.len();
    if !prepared.is_empty() {
        crate::private_fs::ensure_private_home(brain_home)?;
        let manifest = serde_json::to_vec_pretty(&InstallManifest {
            version: 1,
            entries: prepared
                .iter()
                .map(|(_, entry)| ManifestEntry {
                    path: entry.path.clone(),
                    existed: entry.existed,
                    before_sha256: entry.before_sha256.clone(),
                    after_sha256: entry.after_sha256.clone(),
                    backup: entry.backup.clone(),
                    mode: entry.mode,
                })
                .collect(),
        })?;
        if let Err(error) = atomic_write(&manifest_path, &manifest, Some(0o600)) {
            rollback_plans(&prepared);
            return Err(format!("failed to write installation manifest: {error}").into());
        }
    }
    println!("Installed Brain integration in {applied} file(s).");
    Ok(applied)
}

fn rollback_plans(prepared: &[(&PlannedChange, ManifestEntry)]) {
    for (plan, entry) in prepared.iter().rev() {
        if let Some(before) = &plan.before {
            let _ = atomic_write(&plan.path, before, entry.mode);
        } else if plan.path.exists() {
            let _ = fs::remove_file(&plan.path);
        }
    }
}

pub(crate) fn uninstall(
    home: &Path,
    brain_home: &Path,
    dry_run: bool,
    yes: bool,
) -> Result<usize, Box<dyn Error>> {
    crate::private_fs::require_safe_homes(home, brain_home)?;
    let homes = crate::client_home::from_process_env(home);
    uninstall_with(home, brain_home, dry_run, yes, &homes)
}

fn uninstall_with(
    home: &Path,
    brain_home: &Path,
    dry_run: bool,
    yes: bool,
    homes: &ClientHomes,
) -> Result<usize, Box<dyn Error>> {
    // Before the manifest, backups or any client file is looked at.
    crate::private_fs::require_safe_homes(home, brain_home)?;
    let manifest_path = brain_home.join("install-manifest.json");
    // The whole manifest is checked before any client file or backup is read
    // or changed: its location, its file, and every entry against the exact
    // paths and backup names Brain itself writes.
    let refuse = |what: String| -> Box<dyn Error> {
        format!("refusing to uninstall: {what}; nothing was changed").into()
    };
    if !private_dir(brain_home).map_err(|error| refuse(format!("Brain home: {error}")))? {
        println!("No Brain agent installation manifest found; nothing to remove.");
        return Ok(0);
    }
    let Some(bytes) = read_no_follow(&manifest_path, MAX_MANIFEST_BYTES, true)
        .map_err(|error| refuse(format!("install manifest: {error}")))?
    else {
        println!("No Brain agent installation manifest found; nothing to remove.");
        return Ok(0);
    };
    let manifest: InstallManifest = serde_json::from_slice(&bytes)
        .map_err(|error| refuse(format!("install manifest is not valid: {error}")))?;
    validate_manifest(home, &manifest)
        .map_err(|error| refuse(format!("install manifest: {error}")))?;
    let backup_dir = brain_home.join("install-backups");
    let backups_present =
        private_dir(&backup_dir).map_err(|error| refuse(format!("install-backups: {error}")))?;
    // Entries for a relocated client are matched on the exact default paths
    // only and skipped before their current file or backup is read; the
    // manifest, backups and stable binary are then kept.
    let relocated = relocated_clients(homes);
    let guarded = relocated
        .iter()
        .flat_map(|(client, _)| default_client_paths(home, *client))
        .collect::<Vec<_>>();
    for (client, location) in &relocated {
        if manifest
            .entries
            .iter()
            .any(|entry| default_client_paths(home, *client).contains(&entry.path))
        {
            println!("{}", relocation_conflict(*client, *location, "restore"));
        }
    }
    // Pass 1 reads and classifies every eligible entry and verifies each
    // needed backup before anything is changed.
    let mut skipped = false;
    let mut completed = 0_usize;
    let mut planned = Vec::new();
    for entry in &manifest.entries {
        if guarded.contains(&entry.path) {
            skipped = true;
            continue;
        }
        if let Err(reason) = symlink_free(home, &entry.path) {
            println!(
                "CONFLICT {}: {reason}; leaving it and its backup untouched",
                entry.path.display()
            );
            skipped = true;
            continue;
        }
        let current = match read_no_follow(&entry.path, MAX_CLIENT_FILE_BYTES, false) {
            Ok(bytes) => bytes,
            Err(_) => {
                println!(
                    "CONFLICT {}: file cannot be read; leaving it and its backup untouched",
                    entry.path.display()
                );
                skipped = true;
                continue;
            }
        };
        let current_sha = current.as_deref().map(sha256);
        // Restored by an earlier, partial uninstall: the original bytes are
        // back, or a file Brain created is gone. Nothing is rewritten.
        let already_restored = if entry.existed {
            current_sha.is_some() && current_sha == entry.before_sha256
        } else {
            current.is_none()
        };
        if already_restored {
            completed += 1;
            continue;
        }
        if current_sha.as_deref() != Some(entry.after_sha256.as_str()) {
            println!(
                "CONFLICT {}: file changed after installation; leaving it and its backup untouched",
                entry.path.display()
            );
            skipped = true;
            continue;
        }
        let original = if entry.existed {
            let relative = entry.backup.as_ref().ok_or("missing installation backup")?;
            if !backups_present {
                return Err("missing installation backup directory".into());
            }
            let bytes = read_no_follow(&backup_dir.join(relative), MAX_CLIENT_FILE_BYTES, true)
                .map_err(|error| format!("installation backup: {error}"))?
                .ok_or("missing installation backup")?;
            if entry.before_sha256.as_deref() != Some(sha256(&bytes).as_str()) {
                return Err(
                    format!("backup checksum mismatch for {}", entry.path.display()).into(),
                );
            }
            Some(bytes)
        } else {
            None
        };
        planned.push((entry, current, original));
    }
    if completed > 0 {
        println!("{completed} file(s) were already restored by an earlier uninstall.");
    }
    let mut restored = 0_usize;
    for (entry, current, original) in &planned {
        print!(
            "{}",
            render_diff(&entry.path, current.as_deref(), original.as_deref())
        );
        if dry_run {
            continue;
        }
        if !yes && !confirm(&format!("Restore {}?", entry.path.display()))? {
            skipped = true;
            continue;
        }
        match original {
            Some(bytes) => atomic_write(&entry.path, bytes, entry.mode)?,
            None => fs::remove_file(&entry.path)?,
        }
        restored += 1;
    }
    if dry_run {
        println!(
            "Dry run: {} file(s) would be restored; no files written.",
            planned.len()
        );
        return Ok(planned.len());
    }
    if !skipped && restored + completed == manifest.entries.len() {
        for entry in &manifest.entries {
            if let Some(relative) = &entry.backup {
                let path = backup_dir.join(relative);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        if backup_dir.exists() {
            let _ = fs::remove_dir(&backup_dir);
        }
        fs::remove_file(manifest_path)?;
        // The settings shortcut and agent hooks intentionally share one stable
        // binary. Removing one integration must not break the other.
        let copy = stable_copy_path(brain_home);
        if desktop_copy_owner_path(brain_home).exists() {
            println!(
                "Retained the Brain binary at {} because the desktop shortcut still uses it.",
                copy.display()
            );
        } else if copy.exists() && fs::remove_file(&copy).is_ok() {
            if let Some(parent) = copy.parent() {
                let _ = fs::remove_dir(parent);
            }
            println!("Removed the Brain binary at {}.", copy.display());
        }
    }
    println!("Uninstalled Brain integration from {restored} file(s).");
    Ok(restored)
}

/// Clients whose home variable points away from the default location. They
/// are left out of detection and planning entirely; only the variable name and
/// state are reported, never its value.
fn relocated_clients(homes: &ClientHomes) -> Vec<(AgentClient, ClientHome)> {
    [
        (AgentClient::Claude, homes.claude),
        (AgentClient::Codex, homes.codex),
    ]
    .into_iter()
    .filter(|(_, location)| location.state.relocated())
    .collect()
}

/// Names the client, variable and state only; never the variable's value or
/// a path.
fn relocation_conflict(client: AgentClient, location: ClientHome, action: &str) -> String {
    format!(
        "CONFLICT {}: {} is {}; Brain does not follow relocated client homes, so this client's configuration was neither read nor written ({action} skipped).",
        client.name(),
        location.variable,
        location.state.as_str()
    )
}

/// The exact default files `build_plans` writes for a client.
fn default_client_paths(home: &Path, client: AgentClient) -> Vec<PathBuf> {
    match client {
        AgentClient::Claude => vec![
            home.join(".claude.json"),
            home.join(".claude/settings.json"),
            home.join(".claude/CLAUDE.md"),
        ],
        AgentClient::Codex => vec![
            home.join(".codex/config.toml"),
            home.join(".codex/hooks.json"),
            home.join(".codex/AGENTS.md"),
        ],
        AgentClient::Cursor => vec![
            home.join(".cursor/mcp_config.json"),
            home.join(".cursor/rules/brain-for-ai.mdc"),
        ],
    }
}

/// Every path a valid manifest may name: the exact default files of the
/// clients Brain integrates, under the current `HOME`.
fn managed_paths(home: &Path) -> Vec<PathBuf> {
    [AgentClient::Claude, AgentClient::Codex, AgentClient::Cursor]
        .into_iter()
        .flat_map(|client| default_client_paths(home, client))
        .collect()
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Structural check of a parsed manifest, before anything it names is read.
fn validate_manifest(home: &Path, manifest: &InstallManifest) -> Result<(), String> {
    if manifest.version != 1 {
        return Err(format!("unsupported version {}", manifest.version));
    }
    let allowed = managed_paths(home);
    if manifest.entries.len() > allowed.len() {
        return Err("too many entries".into());
    }
    let mut paths = Vec::new();
    let mut backups = Vec::new();
    for (index, entry) in manifest.entries.iter().enumerate() {
        let at = |why: &str| format!("entry {index}: {why}");
        if !allowed.contains(&entry.path) {
            return Err(at("not a file Brain manages"));
        }
        if paths.contains(&&entry.path) {
            return Err(at("duplicate path"));
        }
        paths.push(&entry.path);
        if !is_sha256(&entry.after_sha256) {
            return Err(at("invalid after_sha256"));
        }
        match (entry.existed, &entry.before_sha256, &entry.backup) {
            (true, Some(before), Some(backup)) => {
                if !is_sha256(before) {
                    return Err(at("invalid before_sha256"));
                }
                // Exactly what `install` writes: `{index:03}.bak`, with an
                // index below the number of managed files.
                let name = backup.to_str().filter(|name| {
                    name.len() == 7
                        && name.ends_with(".bak")
                        && name[..3].bytes().all(|b| b.is_ascii_digit())
                        && name[..3].parse::<usize>().is_ok_and(|n| n < allowed.len())
                });
                let Some(name) = name else {
                    return Err(at("invalid backup name"));
                };
                if backups.contains(&name) {
                    return Err(at("duplicate backup"));
                }
                backups.push(name);
            }
            (false, None, None) => {}
            _ => return Err(at("existed, before_sha256 and backup disagree")),
        }
        // A plain file's mode: no setuid, setgid or sticky bits.
        if let Some(mode) = entry.mode {
            let kind = mode & !0o7777;
            if (kind != 0 && kind != 0o100000) || mode & 0o7000 != 0 {
                return Err(at("invalid mode"));
            }
        }
    }
    Ok(())
}

/// `Ok(false)` only for an exact `NotFound`. An existing path must be a real
/// directory (not a symlink) owned by this user with no group or other
/// access. Nothing is chmodded here.
fn private_dir(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(invalid("not a real directory"));
            }
            owner_only(&metadata)?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn owner_only(metadata: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    match crate::private_fs::current_uid() {
        Some(uid) if uid == metadata.uid() => {}
        Some(_) => return Err(invalid("owned by another user")),
        None => return Err(invalid("cannot determine the current user")),
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(invalid("accessible by group or others"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn owner_only(_metadata: &fs::Metadata) -> io::Result<()> {
    Ok(())
}

fn invalid(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

/// Reads a regular file through one bounded handle. `Ok(None)` only for an
/// exact `NotFound`; a symlink, special file, oversize file or (with
/// `private`) a file that is not owner-only is an error. The opened handle
/// must be the inode that was checked.
fn read_no_follow(path: &Path, limit: u64, private: bool) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() {
        return Err(invalid("not a regular file"));
    }
    if private {
        owner_only(&metadata)?;
    }
    if metadata.len() > limit {
        return Err(invalid("too large"));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(invalid("changed while it was opened"));
        }
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("too large"));
    }
    Ok(Some(bytes))
}

/// No symlink between `HOME` (which may itself be an alias) and the target,
/// target included. A missing component ends the walk; any other metadata
/// error is a reason to leave the entry alone, never absence.
fn symlink_free(home: &Path, path: &Path) -> Result<(), &'static str> {
    let relative = path.strip_prefix(home).map_err(|_| "outside HOME")?;
    let mut current = home.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("a symlink is in its path");
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("its path cannot be inspected"),
        }
    }
    Ok(())
}

fn detect_clients(home: &Path, homes: &ClientHomes) -> Vec<AgentClient> {
    let mut clients = Vec::new();
    if !homes.claude.state.relocated()
        && (home.join(".claude").exists() || executable_on_path("claude"))
    {
        clients.push(AgentClient::Claude);
    }
    if !homes.codex.state.relocated()
        && (home.join(".codex").exists() || executable_on_path("codex"))
    {
        clients.push(AgentClient::Codex);
    }
    if home.join(".cursor").exists() || executable_on_path("cursor") {
        clients.push(AgentClient::Cursor);
    }
    clients
}

fn executable_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(name).is_file())
    })
}

fn build_plans(home: &Path, binary: &Path, clients: &[AgentClient]) -> Vec<PlannedChange> {
    let command = binary.to_string_lossy();
    let instructions = format!(
        "{MD_BEGIN}\nUse Brain MCP tools for durable context. Call `memory_search` before decisions, `memory_read` for details, and `memory_write` after durable outcomes. Treat `error` and `gotcha` as first-class failure memories.\n{MD_END}\n"
    );
    let mut plans = Vec::new();
    for client in clients {
        match client {
            AgentClient::Claude => {
                plans.push(json_plan(
                    *client,
                    home.join(".claude.json"),
                    "register user-scoped Brain MCP",
                    |text| patch_mcp_json(text, &command),
                ));
                let path = home.join(".claude/settings.json");
                plans.push(json_plan(
                    *client,
                    path,
                    "register seven fast-path hooks",
                    |text| patch_hooks_json(text, &command, "claude"),
                ));
                plans.push(marker_plan(
                    *client,
                    home.join(".claude/CLAUDE.md"),
                    "add user-scoped memory instructions",
                    &instructions,
                    MD_BEGIN,
                    MD_END,
                    None,
                ));
            }
            AgentClient::Codex => {
                let hooks_path = home.join(".codex/hooks.json");
                let config_block = format!(
                    "{CONFIG_BEGIN}\n[mcp_servers.brain]\ncommand = {}\nargs = [\"serve\"]\n\n[hooks.brain]\nconfig = {}\n{CONFIG_END}\n",
                    toml_string(&command),
                    toml_string(&hooks_path.to_string_lossy())
                );
                plans.push(marker_plan(
                    *client,
                    home.join(".codex/config.toml"),
                    "register MCP and user-scoped hook config",
                    &config_block,
                    CONFIG_BEGIN,
                    CONFIG_END,
                    Some(codex_config_conflicts),
                ));
                plans.push(json_plan(
                    *client,
                    hooks_path,
                    "register seven fast-path hooks",
                    |text| patch_hooks_json(text, &command, "codex"),
                ));
                plans.push(marker_plan(
                    *client,
                    home.join(".codex/AGENTS.md"),
                    "add user-scoped memory instructions",
                    &instructions,
                    MD_BEGIN,
                    MD_END,
                    None,
                ));
            }
            AgentClient::Cursor => {
                plans.push(json_plan(
                    *client,
                    home.join(".cursor/mcp_config.json"),
                    "register MCP (Cursor has no supported global hook config)",
                    |text| patch_mcp_json(text, &command),
                ));
                plans.push(marker_plan(
                    *client,
                    home.join(".cursor/rules/brain-for-ai.mdc"),
                    "add user-scoped memory instructions",
                    &instructions,
                    MD_BEGIN,
                    MD_END,
                    None,
                ));
            }
        }
    }
    plans
}

fn json_plan<F>(client: AgentClient, path: PathBuf, description: &str, patch: F) -> PlannedChange
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let before = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            return PlannedChange {
                client,
                path,
                before: None,
                after: None,
                description: description.to_owned(),
                conflict: Some(
                    "existing JSON configuration could not be read; refusing replacement"
                        .to_owned(),
                ),
            };
        }
    };
    let original = match before.as_deref().map(std::str::from_utf8).transpose() {
        Ok(text) => text.unwrap_or("{}\n"),
        Err(_) => {
            return PlannedChange {
                client,
                path,
                before: before.clone(),
                after: before,
                description: description.to_owned(),
                conflict: Some(
                    "existing JSON configuration is not UTF-8; refusing replacement".to_owned(),
                ),
            };
        }
    };
    match patch(original) {
        Ok(after) => PlannedChange {
            client,
            path,
            before,
            after: Some(after.into_bytes()),
            description: description.to_owned(),
            conflict: None,
        },
        Err(conflict) => PlannedChange {
            client,
            path,
            before: before.clone(),
            after: before,
            description: description.to_owned(),
            conflict: Some(conflict),
        },
    }
}

/// A text check run on the existing file before the marker block is planned.
type MarkerCheck = fn(&str) -> Result<(), String>;

fn marker_plan(
    client: AgentClient,
    path: PathBuf,
    description: &str,
    block: &str,
    begin: &str,
    end: &str,
    check: Option<MarkerCheck>,
) -> PlannedChange {
    let refuse = |before: Option<Vec<u8>>, conflict: &str| PlannedChange {
        client,
        path: path.clone(),
        before: before.clone(),
        after: before,
        description: description.to_owned(),
        conflict: Some(conflict.to_owned()),
    };
    // An unreadable or non-UTF-8 file is never treated as empty: appending a
    // block to "" would replace its bytes.
    let before = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            return refuse(
                None,
                "existing file could not be read; refusing replacement",
            );
        }
    };
    let Ok(original) = before.as_deref().map(std::str::from_utf8).transpose() else {
        return refuse(before, "existing file is not UTF-8; refusing replacement");
    };
    let original = original.unwrap_or_default();
    let planned = check
        .map_or(Ok(()), |check| check(original))
        .and_then(|()| upsert_marker(original, block, begin, end));
    match planned {
        Ok(after) => PlannedChange {
            client,
            path,
            before,
            after: Some(after.into_bytes()),
            description: description.to_owned(),
            conflict: None,
        },
        Err(conflict) => refuse(before, &conflict),
    }
}

/// Refuses a Codex `config.toml` that already defines what the Brain block
/// would add, outside the BRAIN_FOR_AI markers. Appending a second
/// `[mcp_servers.brain]` or `[hooks.brain]` makes the whole file invalid TOML,
/// and without a TOML parser the existing definition cannot be proven
/// equivalent, so the user resolves it by hand.
///
/// Keys are decoded the way TOML defines them (quoted segments, basic-string
/// escapes such as `\u0069`), so `[mcp_servers."bra\u0069n"]` is `brain`.
/// Anything the scanner cannot classify with certainty fails closed: unknown
/// escapes, malformed headers, unclassifiable lines, and marker text other
/// than one whole-line begin/end pair outside any multi-line value. Any
/// triple-quoted string value is refused outright (manual fix), rather than
/// guessing where it ends.
pub(crate) fn codex_config_conflicts(original: &str) -> Result<(), String> {
    let mut table: Vec<String> = Vec::new();
    let mut depth = 0_usize;
    let mut in_block = false;
    let mut had_block = false;
    for raw in original.lines() {
        let line = raw.trim();
        if raw.contains(CONFIG_BEGIN) || raw.contains(CONFIG_END) {
            if depth > 0 {
                return Err(toml_ambiguous(
                    "has a BRAIN_FOR_AI marker inside a multi-line value",
                ));
            }
            if line == CONFIG_BEGIN && !in_block && !had_block {
                in_block = true;
                had_block = true;
                continue;
            }
            if line == CONFIG_END && in_block {
                in_block = false;
                continue;
            }
            return Err(toml_ambiguous(
                "mentions a BRAIN_FOR_AI marker other than one whole-line begin/end pair",
            ));
        }
        if in_block {
            // Only Brain's exact block may sit here: upsert_marker refuses
            // any content that differs from it.
            continue;
        }
        if depth > 0 {
            depth = toml_value_depth(line, depth)?;
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let found = if let Some(rest) = line.strip_prefix("[[") {
            // Any array of tables named mcp_servers/hooks cannot take a
            // [mcp_servers.brain] table beside it.
            let path = toml_header(rest, "]]")?;
            table.clone_from(&path);
            matches!(
                path.first().map(String::as_str),
                Some("mcp_servers" | "hooks")
            )
            .then_some(path)
        } else if let Some(rest) = line.strip_prefix('[') {
            let path = toml_header(rest, "]")?;
            table.clone_from(&path);
            (path.len() >= 2 && reserved_brain_path(&path)).then_some(path)
        } else if let Some(equals) = unquoted_index(line, '=') {
            let mut path = table.clone();
            path.extend(toml_key_path(&line[..equals])?);
            let value = line[equals + 1..].trim_start();
            // Triple-quoted strings are never delimited by guesswork: a `"""`
            // in a comment or an escape would hide or reveal headers. Refuse.
            if value.starts_with("\u{22}\u{22}\u{22}") || value.starts_with("\u{27}\u{27}\u{27}") {
                return Err(toml_ambiguous(
                    "has a triple-quoted (multi-line) string value, which Brain does not delimit",
                ));
            }
            depth = toml_value_depth(value, 0)?;
            reserved_brain_path(&path).then_some(path)
        } else {
            return Err(toml_ambiguous(
                "has a line that is not a comment, table header or key/value pair",
            ));
        };
        if let Some(path) = found {
            return Err(format!(
                "`{}` is already defined outside the BRAIN_FOR_AI markers; adding the Brain \
                 block would duplicate it and make config.toml invalid. Remove that definition, \
                 or wrap an identical Brain block in `{CONFIG_BEGIN}` / `{CONFIG_END}` yourself, \
                 then rerun",
                path.join(".")
            ));
        }
    }
    if in_block || depth > 0 {
        return Err(toml_ambiguous(
            "ends inside an unterminated marker block or multi-line value",
        ));
    }
    Ok(())
}

/// A key path the Brain block would collide with.
fn reserved_brain_path(path: &[String]) -> bool {
    matches!(
        path.first().map(String::as_str),
        Some("mcp_servers" | "hooks")
    ) && path.get(1).is_none_or(|second| second == "brain")
}

fn toml_ambiguous(why: &str) -> String {
    format!(
        "config.toml {why}; Brain cannot prove that adding its block would not duplicate \
         `mcp_servers.brain` or `hooks.brain`. Simplify that part by hand, then rerun"
    )
}

/// Parses the key inside a `[..]` / `[[..]]` header; only a comment may follow.
fn toml_header(rest: &str, close: &str) -> Result<Vec<String>, String> {
    let end = unquoted_index(rest, ']')
        .ok_or_else(|| toml_ambiguous("has an unterminated table header"))?;
    let tail = rest[end..]
        .strip_prefix(close)
        .ok_or_else(|| toml_ambiguous("has a malformed table header"))?
        .trim_start();
    if !(tail.is_empty() || tail.starts_with('#')) {
        return Err(toml_ambiguous("has text after a table header"));
    }
    toml_key_path(&rest[..end])
}

/// Decodes a TOML key (`a."b\u0063".'d'`) into its segments, exactly as TOML
/// defines them. Anything outside the spec is refused rather than guessed.
fn toml_key_path(key: &str) -> Result<Vec<String>, String> {
    let undecodable = || toml_ambiguous("has a key Brain cannot decode");
    let chars = key.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut segments = Vec::new();
    let skip_blank = |index: &mut usize| {
        while chars.get(*index).is_some_and(|c| *c == ' ' || *c == '\t') {
            *index += 1;
        }
    };
    loop {
        skip_blank(&mut index);
        let mut segment = String::new();
        match chars.get(index).copied() {
            Some('"') => {
                index += 1;
                loop {
                    let character = *chars.get(index).ok_or_else(undecodable)?;
                    index += 1;
                    match character {
                        '"' => break,
                        '\\' => {
                            let escape = *chars.get(index).ok_or_else(undecodable)?;
                            index += 1;
                            let hex = |count: usize, index: &mut usize| {
                                let digits = chars.get(*index..*index + count)?;
                                *index += count;
                                u32::from_str_radix(&digits.iter().collect::<String>(), 16)
                                    .ok()
                                    .and_then(char::from_u32)
                            };
                            segment.push(match escape {
                                'b' => '\u{8}',
                                't' => '\t',
                                'n' => '\n',
                                'f' => '\u{c}',
                                'r' => '\r',
                                'e' => '\u{1b}',
                                '"' => '"',
                                '\\' => '\\',
                                'u' => hex(4, &mut index).ok_or_else(undecodable)?,
                                'U' => hex(8, &mut index).ok_or_else(undecodable)?,
                                'x' => hex(2, &mut index).ok_or_else(undecodable)?,
                                _ => return Err(undecodable()),
                            });
                        }
                        other => segment.push(other),
                    }
                }
            }
            Some('\u{27}') => {
                index += 1;
                loop {
                    let character = *chars.get(index).ok_or_else(undecodable)?;
                    index += 1;
                    if character == '\u{27}' {
                        break;
                    }
                    segment.push(character);
                }
            }
            _ => {
                while let Some(character) = chars
                    .get(index)
                    .filter(|c| c.is_ascii_alphanumeric() || **c == '_' || **c == '-')
                {
                    segment.push(*character);
                    index += 1;
                }
                if segment.is_empty() {
                    return Err(undecodable());
                }
            }
        }
        segments.push(segment);
        skip_blank(&mut index);
        match chars.get(index) {
            None => return Ok(segments),
            Some('.') => index += 1,
            Some(_) => return Err(undecodable()),
        }
    }
}

/// Byte index of the first `needle` outside a TOML string; `#` outside a
/// string starts a comment and ends the search.
fn unquoted_index(line: &str, needle: char) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\u{27}' => quote = Some(character),
            None if character == '#' => return None,
            None if character == needle => return Some(index),
            None => {}
        }
    }
    None
}

/// Bracket depth after `text`, starting from `depth`, for values that span
/// lines (arrays, inline tables). Strings and comments are skipped.
fn toml_value_depth(text: &str, mut depth: usize) -> Result<usize, String> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (index, &character) in chars.iter().enumerate() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None => match character {
                '"' | '\u{27}' => {
                    if chars.get(index + 1) == Some(&character)
                        && chars.get(index + 2) == Some(&character)
                    {
                        return Err(toml_ambiguous(
                            "has a multi-line string inside a multi-line value",
                        ));
                    }
                    quote = Some(character);
                }
                '#' => break,
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth = depth
                        .checked_sub(1)
                        .ok_or_else(|| toml_ambiguous("has an unbalanced bracket"))?;
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err(toml_ambiguous("has an unterminated string"));
    }
    Ok(depth)
}

fn upsert_marker(original: &str, block: &str, begin: &str, end: &str) -> Result<String, String> {
    match (original.find(begin), original.find(end)) {
        (None, None) => {
            let separator = if original.is_empty() || original.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            Ok(format!("{original}{separator}{block}"))
        }
        (Some(start), Some(end_start)) if end_start >= start => {
            let end_index = end_start + end.len();
            let existing = &original[start..end_index];
            let desired = block.trim_end();
            if existing == desired {
                Ok(original.to_owned())
            } else {
                Err("existing BRAIN_FOR_AI marker block differs; refusing to overwrite manual edits".to_owned())
            }
        }
        _ => Err("unbalanced BRAIN_FOR_AI markers".to_owned()),
    }
}

fn patch_mcp_json(original: &str, binary: &str) -> Result<String, String> {
    let mut text = ensure_json_object(original)?;
    let entry = format!(
        "\"brain\": {{\"command\": {}, \"args\": [\"serve\"]}}",
        serde_json::to_string(binary).map_err(|error| error.to_string())?
    );
    let root = object_bounds(&text, 0)?;
    // Only the root member counts: `projects.<dir>.mcpServers` is per-project
    // configuration and must never be mistaken for the user-scoped entry.
    if let Some((start, _)) = property_value(&text, root, "mcpServers")? {
        if text.as_bytes().get(start) != Some(&b'{') {
            return Err("mcpServers is not a JSON object; refusing to edit it".to_owned());
        }
        let servers = object_bounds(&text, start)?;
        if let Some((brain_start, brain_end)) = property_value(&text, servers, "brain")? {
            let current = parse_jsonc(&text[brain_start..brain_end]).map_err(|_| {
                "mcpServers.brain is not valid JSON; refusing to edit it".to_owned()
            })?;
            if !brain_entry_compatible(&current, binary) {
                return Err(format!(
                    "mcpServers.brain already exists with different settings; expected command \
                     {} with args [\"serve\"] (or [\"--agent\", <name>, \"serve\"]) and no other \
                     keys. Edit or remove it manually",
                    serde_json::to_string(binary).unwrap_or_default()
                ));
            }
        } else {
            text = insert_object_member(&text, servers, &entry);
        }
    } else {
        let value = format!("\"mcpServers\": {{{entry}}}");
        text = insert_object_member(&text, root, &value);
    }
    let planned =
        parse_jsonc(&text).map_err(|error| format!("planned JSON is invalid: {error}"))?;
    if !planned
        .pointer("/mcpServers/brain")
        .is_some_and(|value| brain_entry_compatible(value, binary))
    {
        return Err("planned JSON does not carry the Brain entry at the root; refusing".to_owned());
    }
    Ok(text)
}

/// Whether an existing `mcpServers.brain` value is this binary's stdio server.
///
/// Compared on parsed values, never by substring: a command that merely
/// contains the path, or the path appearing in another field, is different.
fn brain_entry_compatible(value: &serde_json::Value, binary: &str) -> bool {
    let Some(entry) = value.as_object() else {
        return false;
    };
    if entry
        .keys()
        .any(|key| !matches!(key.as_str(), "command" | "args" | "type"))
        || entry.get("type").is_some_and(|kind| kind != "stdio")
        || entry.get("command").and_then(serde_json::Value::as_str) != Some(binary)
    {
        return false;
    }
    let args = entry
        .get("args")
        .and_then(serde_json::Value::as_array)
        .and_then(|args| {
            args.iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()
        });
    match args.as_deref() {
        Some(["serve"]) => true,
        Some(["--agent", agent, "serve"]) => !agent.is_empty() && !agent.starts_with('-'),
        _ => false,
    }
}

/// Whether an event's hook array already runs exactly `command`.
fn hook_registered(groups: &serde_json::Value, command: &str) -> bool {
    groups.as_array().is_some_and(|groups| {
        groups.iter().any(|group| {
            group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook.get("type").and_then(serde_json::Value::as_str) == Some("command")
                            && hook.get("command").and_then(serde_json::Value::as_str)
                                == Some(command)
                    })
                })
        })
    })
}

const HOOK_EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PreCompact",
    "Stop",
    "SessionEnd",
];

fn patch_hooks_json(original: &str, binary: &str, client: &str) -> Result<String, String> {
    let mut text = ensure_json_object(original)?;
    let hook_binary = shell_quote(binary);
    let root = object_bounds(&text, 0)?;
    match property_value(&text, root, "hooks")? {
        None => text = insert_object_member(&text, root, "\"hooks\": {}"),
        Some((start, _)) if text.as_bytes().get(start) != Some(&b'{') => {
            return Err("hooks is not a JSON object; refusing to edit it".to_owned());
        }
        Some(_) => {}
    }
    for event in HOOK_EVENTS {
        let root = object_bounds(&text, 0)?;
        let (hooks_start, _) = property_value(&text, root, "hooks")?
            .ok_or_else(|| "hooks object insertion failed".to_owned())?;
        let hooks = object_bounds(&text, hooks_start)?;
        let command = format!("{hook_binary} hook --event {event} --client {client}");
        let item = format!(
            "{{\"hooks\":[{{\"type\":\"command\",\"command\":{}}}]}}",
            serde_json::to_string(&command).map_err(|error| error.to_string())?
        );
        if let Some((event_start, event_end)) = property_value(&text, hooks, event)? {
            if text.as_bytes().get(event_start) != Some(&b'[') {
                return Err(format!(
                    "hooks.{event} is not a JSON array; refusing to edit it"
                ));
            }
            let current = parse_jsonc(&text[event_start..event_end])
                .map_err(|_| format!("hooks.{event} is not valid JSON; refusing to edit it"))?;
            if hook_registered(&current, &command) {
                continue;
            }
            let array = array_bounds(&text, event_start)?;
            text = insert_array_item(&text, array, &item);
        } else {
            text = insert_object_member(
                &text,
                hooks,
                &format!(
                    "{}: [{item}]",
                    serde_json::to_string(event).map_err(|error| error.to_string())?
                ),
            );
        }
    }
    let planned =
        parse_jsonc(&text).map_err(|error| format!("planned JSON is invalid: {error}"))?;
    for event in HOOK_EVENTS {
        let command = format!("{hook_binary} hook --event {event} --client {client}");
        if !planned
            .get("hooks")
            .and_then(|hooks| hooks.get(event))
            .is_some_and(|groups| hook_registered(groups, &command))
        {
            return Err(format!(
                "planned JSON lacks the hooks.{event} entry; refusing"
            ));
        }
    }
    Ok(text)
}

/// Parses JSON that may carry `//` and `/* */` comments (nothing else beyond
/// strict JSON). Comments become spaces, so byte positions are kept.
fn parse_jsonc(text: &str) -> Result<serde_json::Value, String> {
    let bytes = text.as_bytes();
    let mut stripped = Vec::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            while index < bytes.len() && bytes[index] != b'\n' {
                stripped.push(b' ');
                index += 1;
            }
            continue;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let close = text[index + 2..]
                .find("*/")
                .map_or(bytes.len(), |offset| index + 2 + offset + 2);
            stripped.extend(
                bytes[index..close]
                    .iter()
                    .map(|&byte| if byte == b'\n' { b'\n' } else { b' ' }),
            );
            index = close;
            continue;
        }
        stripped.push(byte);
        index += 1;
    }
    serde_json::from_slice(&stripped).map_err(|error| error.to_string())
}

fn ensure_json_object(original: &str) -> Result<String, String> {
    if original.trim().is_empty() {
        return Ok("{}\n".to_owned());
    }
    // The whole file must already be one JSON object (comments allowed): a
    // byte-level edit of anything else could not be proven valid.
    if !parse_jsonc(original).is_ok_and(|value| value.is_object()) {
        return Err(
            "existing configuration is not a valid JSON object; refusing to edit it".to_owned(),
        );
    }
    let text = original.to_owned();
    object_bounds(&text, 0)?;
    Ok(text)
}

fn object_bounds(text: &str, start_at: usize) -> Result<(usize, usize), String> {
    // The value must START here (after whitespace/comments): a `{` found
    // further on, e.g. inside a comment, is not this value.
    let start = skip_insignificant(text.as_bytes(), start_at, text.len());
    if text.as_bytes().get(start) != Some(&b'{') {
        return Err("expected JSON object".to_owned());
    }
    let end = matching_delimiter(text, start, b'{', b'}')?;
    Ok((start, end))
}

fn array_bounds(text: &str, start_at: usize) -> Result<(usize, usize), String> {
    let start = skip_insignificant(text.as_bytes(), start_at, text.len());
    if text.as_bytes().get(start) != Some(&b'[') {
        return Err("expected JSON array".to_owned());
    }
    let end = matching_delimiter(text, start, b'[', b']')?;
    Ok((start, end))
}

fn matching_delimiter(text: &str, start: usize, open: u8, close: u8) -> Result<usize, String> {
    let bytes = text.as_bytes();
    let mut depth = 0_i64;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = start;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if byte == open {
            depth += 1;
        } else if byte == close {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        } else if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        index += 1;
    }
    Err("unbalanced JSON delimiters".to_owned())
}

/// Skips whitespace and comments from `index`, never past `limit`.
fn skip_insignificant(bytes: &[u8], mut index: usize, limit: usize) -> usize {
    while index < limit {
        match bytes[index] {
            b' ' | b'\t' | b'\n' | b'\r' => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < limit && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < limit && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                    index += 1;
                }
                index = (index + 2).min(limit);
            }
            _ => break,
        }
    }
    index
}

/// Index just past the string literal opening at `start`.
fn string_end(bytes: &[u8], start: usize, limit: usize) -> Option<usize> {
    let mut index = start + 1;
    let mut escaped = false;
    while index < limit {
        if escaped {
            escaped = false;
        } else if bytes[index] == b'\\' {
            escaped = true;
        } else if bytes[index] == b'"' {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

/// Finds the value of the DIRECT member `key` of the object spanning
/// `bounds` (`{` .. `}`). Members of nested objects, and `"key"` text inside
/// string values, are skipped: the scan walks member by member and compares
/// decoded key names. A key that appears twice is refused, because
/// serde_json (and the agent) would use the LAST copy while an edit might
/// look at the first.
fn property_value(
    text: &str,
    bounds: (usize, usize),
    key: &str,
) -> Result<Option<(usize, usize)>, String> {
    let malformed = || format!("could not scan the object holding `{key}`; refusing to edit");
    let bytes = text.as_bytes();
    let limit = bounds.1;
    let mut found = None;
    let mut index = skip_insignificant(bytes, bounds.0 + 1, limit);
    while index < limit {
        if bytes[index] != b'"' {
            return Err(malformed());
        }
        let key_end = string_end(bytes, index, limit).ok_or_else(malformed)?;
        let name: String = serde_json::from_str(&text[index..key_end]).map_err(|_| malformed())?;
        index = skip_insignificant(bytes, key_end, limit);
        if bytes.get(index) != Some(&b':') {
            return Err(malformed());
        }
        let start = skip_insignificant(bytes, index + 1, limit);
        let end = match bytes.get(start).ok_or_else(malformed)? {
            b'{' => matching_delimiter(text, start, b'{', b'}').map_err(|_| malformed())? + 1,
            b'[' => matching_delimiter(text, start, b'[', b']').map_err(|_| malformed())? + 1,
            b'"' => string_end(bytes, start, limit).ok_or_else(malformed)?,
            _ => {
                let mut end = start;
                while end < limit && !b",}] \t\r\n/".contains(&bytes[end]) {
                    end += 1;
                }
                end
            }
        };
        if end > limit {
            return Err(malformed());
        }
        if name == key {
            if found.is_some() {
                return Err(format!(
                    "`{key}` appears more than once in the same JSON object; refusing to edit \
                     an ambiguous file"
                ));
            }
            found = Some((start, end));
        }
        index = skip_insignificant(bytes, end, limit);
        if index >= limit {
            break;
        }
        if bytes[index] != b',' {
            return Err(malformed());
        }
        index = skip_insignificant(bytes, index + 1, limit);
    }
    Ok(found)
}

fn insert_object_member(text: &str, bounds: (usize, usize), member: &str) -> String {
    insert_delimited(text, bounds, member)
}

fn insert_array_item(text: &str, bounds: (usize, usize), item: &str) -> String {
    insert_delimited(text, bounds, item)
}

fn insert_delimited(text: &str, bounds: (usize, usize), value: &str) -> String {
    let close = bounds.1;
    let whitespace_start = text[..close]
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_whitespace())
        .map_or(bounds.0 + 1, |(index, character)| {
            index + character.len_utf8()
        });
    let has_content = text[bounds.0 + 1..whitespace_start]
        .chars()
        .any(|character| !character.is_whitespace());
    let needs_comma = has_content
        && text[..whitespace_start]
            .chars()
            .next_back()
            .is_some_and(|character| character != ',');
    let line_start = text[..close].rfind('\n').map_or(0, |index| index + 1);
    let indent = text[line_start..close]
        .chars()
        .take_while(|character| character.is_whitespace())
        .collect::<String>();
    let child_indent = format!("{indent}  ");
    let trailing = &text[whitespace_start..close];
    let closing_space = if trailing.contains('\n') {
        trailing.to_owned()
    } else {
        format!("\n{indent}")
    };
    format!(
        "{}{}\n{child_indent}{value}{closing_space}{}",
        &text[..whitespace_start],
        if needs_comma { "," } else { "" },
        &text[close..]
    )
}

fn render_diff(path: &Path, before: Option<&[u8]>, after: Option<&[u8]>) -> String {
    let before = before.map_or_else(String::new, |bytes| {
        String::from_utf8_lossy(bytes).into_owned()
    });
    let after = after.map_or_else(String::new, |bytes| {
        String::from_utf8_lossy(bytes).into_owned()
    });
    let old = before.lines().collect::<Vec<_>>();
    let new = after.lines().collect::<Vec<_>>();
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let context_start = prefix.saturating_sub(2);
    let old_end = old.len().saturating_sub(suffix).min(prefix + 40);
    let new_end = new.len().saturating_sub(suffix).min(prefix + 40);
    let mut output = format!(
        "--- {}.before\n+++ {}.after\n@@ -{},{} +{},{} @@\n",
        path.display(),
        path.display(),
        context_start + 1,
        old_end.saturating_sub(context_start),
        context_start + 1,
        new_end.saturating_sub(context_start)
    );
    for line in &old[context_start..prefix] {
        output.push_str(&format!(" {}\n", bounded_line(line)));
    }
    for line in &old[prefix..old_end] {
        output.push_str(&format!("-{}\n", bounded_line(line)));
    }
    for line in &new[prefix..new_end] {
        output.push_str(&format!("+{}\n", bounded_line(line)));
    }
    if old_end < old.len().saturating_sub(suffix) || new_end < new.len().saturating_sub(suffix) {
        output.push_str(" ... diff truncated ...\n");
    }
    output
}

fn bounded_line(line: &str) -> String {
    if line.chars().count() <= 300 {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(299).collect::<String>())
    }
}

#[cfg(all(test, unix))]
mod home_guard_tests {
    use super::*;
    use crate::private_fs::UNSAFE_HOME;
    use crate::private_fs::tests::{snapshot, unsafe_home_pairs};
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-installer-guard-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn unusable_homes_refuse_install_and_uninstall_before_any_io() {
        let root = scratch("refuse");
        // A detectable client, an existing manifest and a source binary that a
        // loose guard would read, back up, copy or restore.
        let good_home = root.join("ok");
        fs::create_dir_all(good_home.join(".codex")).unwrap();
        fs::write(good_home.join(".codex/config.toml"), b"model = \"x\"\n").unwrap();
        let source = root.join("build/brain");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let homes = crate::client_home::classify_all(&good_home, None, None, None);
        let before = snapshot(&root);
        for (home, brain_home) in unsafe_home_pairs(&root) {
            for dry_run in [true, false] {
                for result in [
                    install_with(&home, &brain_home, &source, dry_run, true, &homes),
                    uninstall_with(&home, &brain_home, dry_run, true, &homes),
                    install(&home, &brain_home, &source, dry_run, true),
                    uninstall(&home, &brain_home, dry_run, true),
                ] {
                    assert_eq!(result.unwrap_err().to_string(), UNSAFE_HOME, "{home:?}");
                }
            }
        }
        assert_eq!(
            snapshot(&root),
            before,
            "no manifest, backup, copy or client write"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_unicode_home_with_spaces_still_plans_normally() {
        let root = scratch("unicode");
        let home = root.join("дом ñ space");
        fs::create_dir_all(home.join(".codex")).unwrap();
        let source = root.join("build/brain");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let homes = crate::client_home::classify_all(&home, None, None, None);
        let before = snapshot(&root);
        install_with(&home, &home.join(".brain"), &source, true, true, &homes).unwrap();
        uninstall_with(&home, &home.join(".brain"), true, true, &homes).unwrap();
        assert_eq!(snapshot(&root), before, "dry runs still write nothing");
        let _ = fs::remove_dir_all(root);
    }
}

/// Where the agent hooks are pointed: a copy Brain owns, inside its own home.
pub(crate) fn stable_copy_path(brain_home: &Path) -> PathBuf {
    brain_home.join("bin").join("brain")
}

/// Marker proving that the desktop shortcut owns a reference to the shared
/// stable binary. A stale marker only retains a harmless file; a missing one
/// could break a user's launcher, so desktop installation writes it first.
pub(crate) fn desktop_copy_owner_path(brain_home: &Path) -> PathBuf {
    brain_home.join(DESKTOP_COPY_OWNER)
}

/// Places the running binary where the hooks can always reach it.
///
/// Hooks must never point at a build directory. Rebuilding the project relinks
/// that file, and for the seconds the linker holds it every connected agent
/// calls a binary that is missing or half-written — observed live as a shell
/// that refused every command. `cargo clean`, `git clean`, or moving the
/// checkout would break them permanently.
pub(crate) fn install_stable_copy(
    binary: &Path,
    destination: &Path,
) -> Result<bool, Box<dyn Error>> {
    validate_versionless_binary(binary)?;
    if same_path(binary, destination) {
        validate_stable_copy(destination)?;
        return Ok(false);
    }
    let bytes = fs::read(binary).map_err(|error| {
        format!(
            "failed to read the Brain binary at {}: {error}",
            binary.display()
        )
    })?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    // Replacing by rename keeps a running copy alive on its old inode, so an
    // agent mid-hook finishes against the binary it started with.
    atomic_write(destination, &bytes, Some(0o755)).map_err(|error| {
        format!(
            "failed to install the Brain binary to {}: {error}",
            destination.display()
        )
    })?;
    validate_stable_copy(destination)?;
    Ok(true)
}

fn validate_stable_copy(path: &Path) -> Result<(), Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "failed to verify the Brain binary at {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "stable Brain binary is not a regular file: {}",
            path.display()
        )
        .into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(
                format!("stable Brain binary is not executable: {}", path.display()).into(),
            );
        }
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn validate_versionless_binary(binary: &Path) -> Result<(), Box<dyn Error>> {
    match binary.file_name().and_then(OsStr::to_str) {
        Some("brain" | "brain.exe") => Ok(()),
        _ => Err(format!(
            "hook binary path must end in versionless `brain`, got {}",
            binary.display()
        )
        .into()),
    }
}

/// Refuses to write a file that Git would carry into a commit.
///
/// The danger is committing personal agent settings into a shared repository,
/// as CodeGraph did. Being inside a worktree is not enough: a file the user
/// already ignores cannot reach a commit, and refusing there would block people
/// whose home directory happens to be versioned.
fn ensure_not_git_managed(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut current = path.parent();
    while let Some(directory) = current {
        let dot_git = directory.join(".git");
        let real_repository = dot_git.is_file() || dot_git.join("HEAD").is_file();
        if real_repository {
            if git_would_ignore(directory, path) {
                return Ok(());
            }
            return Err(format!(
                "refusing to write {} because Git in {} would include it in a commit; \
                 add it to .gitignore or point Brain at a user-scoped config instead",
                path.display(),
                directory.display()
            )
            .into());
        }
        current = directory.parent();
    }
    Ok(())
}

/// Asks Git whether the path is ignored in that repository.
///
/// A failure to run Git is treated as "not ignored": when the answer is
/// unknown, the safe assumption is that the file would be committed.
fn git_would_ignore(repository: &Path, path: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repository)
        .arg("check-ignore")
        .arg("--quiet")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // The process id alone is shared by every thread, so two concurrent writes
    // to the same destination raced for one temporary file and the loser
    // renamed a half-written binary into place — surfacing as "Text file busy"
    // when it was immediately executed. A per-call counter makes the name
    // unique within the process as well as between processes.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("brain-tmp-{}-{nonce}", std::process::id()));
    fs::write(&temporary, bytes)?;
    set_file_mode(&temporary, mode)?;
    fs::rename(temporary, path)
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut schedule = [0_u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temporary1 = h
                .wrapping_add(sum1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temporary2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temporary1);
            d = c;
            c = b;
            b = a;
            a = temporary1.wrapping_add(temporary2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    state
        .iter()
        .map(|word| format!("{word:08x}"))
        .collect::<String>()
}

fn confirm(question: &str) -> io::Result<bool> {
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

pub(crate) fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "/._-".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode())
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Tests inject client-home variables instead of reading or mutating the
    /// process environment, so parallel tests cannot race on it.
    fn unset_homes() -> ClientHomes {
        crate::client_home::classify_all(Path::new("/synthetic/home"), None, None, None)
    }

    fn guard_fixture(label: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = compat_root(label);
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        // Stale default-location files with Brain markers already present.
        fs::write(home.join(".claude.json"), b"{\"mcpServers\": {}}\n").unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            b"# stale\n# BRAIN_FOR_AI_BEGIN\n[mcp_servers.brain]\ncommand = \"/old/brain\"\n# BRAIN_FOR_AI_END\n",
        )
        .unwrap();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let binary = build.join("brain");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        (root, home, brain_home, binary)
    }

    fn snapshot(home: &Path, files: &[&str]) -> Vec<Option<Vec<u8>>> {
        files.iter().map(|f| fs::read(home.join(f)).ok()).collect()
    }

    const CODEX_FILES: [&str; 3] = [
        ".codex/config.toml",
        ".codex/hooks.json",
        ".codex/AGENTS.md",
    ];
    const CLAUDE_FILES: [&str; 3] = [".claude.json", ".claude/settings.json", ".claude/CLAUDE.md"];

    #[test]
    fn relocated_codex_is_skipped_even_with_yes_and_claude_still_installs() {
        let (root, home, brain_home, binary) = guard_fixture("guard-codex");
        let forbidden = root.join("forbidden-codex-home");
        for raw in [OsStr::new("relative/codex"), forbidden.as_os_str()] {
            let homes = crate::client_home::classify_all(&home, None, Some(raw), None);
            assert!(homes.codex.state.relocated());
            assert_eq!(build_plans_for(&home, &homes), [AgentClient::Claude]);
        }
        let homes =
            crate::client_home::classify_all(&home, None, Some(forbidden.as_os_str()), None);
        let codex_before = snapshot(&home, &CODEX_FILES);
        let claude_before = snapshot(&home, &CLAUDE_FILES);
        let applied =
            install_with(&home, &brain_home, &binary, false, true, &homes).expect("install");
        assert!(applied > 0, "the unrelated client still installs");
        assert_eq!(
            snapshot(&home, &CODEX_FILES),
            codex_before,
            "guarded default left byte-identical"
        );
        assert_ne!(snapshot(&home, &CLAUDE_FILES), claude_before);
        assert!(
            stable_copy_path(&brain_home).exists(),
            "core install continues"
        );
        let manifest = fs::read_to_string(brain_home.join("install-manifest.json")).unwrap();
        assert!(
            !manifest.contains(".codex"),
            "no restoration entry for the relocated client"
        );
        assert!(!manifest.contains("forbidden"));
        assert!(
            !forbidden.exists(),
            "the override location is never created"
        );
        let observed = crate::doctor::document_with(&home, &brain_home, 1, &homes);
        assert_eq!(
            observed["clients"][1]["mcp_config_entry_present"],
            serde_json::Value::Null
        );
        assert_eq!(observed["clients"][0]["mcp_config_entry_present"], true);
        assert!(!observed.to_string().contains("forbidden"));
        assert!(!forbidden.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relocated_claude_leaves_every_default_claude_file_alone() {
        let (root, home, brain_home, binary) = guard_fixture("guard-claude");
        let homes = crate::client_home::classify_all(
            &home,
            Some(OsStr::new("/synthetic/claude-elsewhere")),
            None,
            None,
        );
        let claude_before = snapshot(&home, &CLAUDE_FILES);
        install_with(&home, &brain_home, &binary, true, true, &homes).expect("dry run");
        install_with(&home, &brain_home, &binary, false, true, &homes).expect("install");
        assert_eq!(snapshot(&home, &CLAUDE_FILES), claude_before);
        let manifest = fs::read_to_string(brain_home.join("install-manifest.json")).unwrap();
        assert!(manifest.contains(".codex/hooks.json"), "Codex proceeds");
        assert!(!manifest.contains(".claude"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_spellings_keep_the_legacy_plan() {
        let (root, home, _, _) = guard_fixture("guard-default");
        let claude_same = format!("{}//.claude/", home.display());
        let codex_same = format!("{}/./.codex/", home.display());
        for (claude, codex) in [
            (None, None),
            (Some(OsStr::new("")), Some(OsStr::new(""))),
            (
                Some(OsStr::new(&claude_same)),
                Some(OsStr::new(&codex_same)),
            ),
        ] {
            let homes = crate::client_home::classify_all(&home, claude, codex, None);
            assert!(relocated_clients(&homes).is_empty());
            assert_eq!(
                build_plans_for(&home, &homes),
                [AgentClient::Claude, AgentClient::Codex]
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_client_paths_match_the_install_plan() {
        let (root, home, _, _) = guard_fixture("guard-paths");
        for client in [AgentClient::Claude, AgentClient::Codex, AgentClient::Cursor] {
            let planned = build_plans(&home, Path::new("/opt/brain"), &[client])
                .into_iter()
                .map(|plan| plan.path)
                .collect::<Vec<_>>();
            assert_eq!(planned, default_client_paths(&home, client));
        }
        let _ = fs::remove_dir_all(root);
    }

    fn installed_fixture(label: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (root, home, brain_home, binary) = guard_fixture(label);
        fs::create_dir_all(home.join(".cursor")).unwrap();
        fs::write(
            home.join(".cursor/mcp_config.json"),
            b"{\"mcpServers\":{}}\n",
        )
        .unwrap();
        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).expect("install");
        (root, home, brain_home)
    }

    fn brain_state(brain_home: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![brain_home.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn uninstall_skips_relocated_clients_and_keeps_manifest_backups_and_binary() {
        let (root, home, brain_home) = installed_fixture("unguard-both");
        let installed_claude = snapshot(&home, &CLAUDE_FILES);
        let installed_codex = snapshot(&home, &CODEX_FILES);
        let forbidden = root.join("forbidden-claude-home");
        let homes = crate::client_home::classify_all(
            &home,
            Some(forbidden.as_os_str()),
            Some(OsStr::new("relative/codex-secret")),
            None,
        );
        let before = brain_state(&brain_home);

        uninstall_with(&home, &brain_home, true, true, &homes).expect("dry run");
        assert_eq!(brain_state(&brain_home), before, "dry run writes nothing");
        assert_eq!(snapshot(&home, &CLAUDE_FILES), installed_claude);

        let restored = uninstall_with(&home, &brain_home, false, true, &homes).expect("uninstall");
        assert_eq!(restored, 2, "only the two Cursor entries are eligible");
        assert_eq!(
            snapshot(&home, &CLAUDE_FILES),
            installed_claude,
            "Claude untouched"
        );
        assert_eq!(
            snapshot(&home, &CODEX_FILES),
            installed_codex,
            "Codex untouched"
        );
        assert_eq!(
            fs::read(home.join(".cursor/mcp_config.json")).unwrap(),
            b"{\"mcpServers\":{}}\n",
            "Cursor restored byte for byte"
        );
        assert!(!home.join(".cursor/rules/brain-for-ai.mdc").exists());
        let manifest = brain_home.join("install-manifest.json");
        assert!(manifest.exists(), "manifest kept while entries are skipped");
        let kept = brain_state(&brain_home);
        for (path, bytes) in &before {
            if path.starts_with(brain_home.join("install-backups")) || *path == manifest {
                assert!(
                    kept.contains(&(path.clone(), bytes.clone())),
                    "{path:?} kept"
                );
            }
        }
        assert!(stable_copy_path(&brain_home).exists(), "stable binary kept");
        assert!(!forbidden.exists());
        for (client, location) in relocated_clients(&homes) {
            let line = relocation_conflict(client, location, "restore");
            assert!(!line.contains("forbidden") && !line.contains("secret") && !line.contains('/'));
            assert!(line.contains(location.variable));
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_with_only_claude_relocated_restores_codex_and_cursor() {
        let (root, home, brain_home) = installed_fixture("unguard-claude");
        let installed_claude = snapshot(&home, &CLAUDE_FILES);
        let homes = crate::client_home::classify_all(
            &home,
            Some(OsStr::new("/synthetic/claude-elsewhere")),
            None,
            None,
        );
        let restored = uninstall_with(&home, &brain_home, false, true, &homes).expect("uninstall");
        assert!(restored >= 3, "Codex and Cursor entries restored");
        assert_eq!(snapshot(&home, &CLAUDE_FILES), installed_claude);
        assert!(
            !home.join(".codex/hooks.json").exists(),
            "created Codex file removed"
        );
        assert!(brain_home.join("install-manifest.json").exists());
        assert!(stable_copy_path(&brain_home).exists());
        let _ = fs::remove_dir_all(root);
    }

    const CURSOR_BEFORE: &[u8] = b"{\"mcpServers\":{}}\n";

    fn partially_uninstalled(label: &str) -> (PathBuf, PathBuf, PathBuf) {
        let (root, home, brain_home) = installed_fixture(label);
        let relocated = crate::client_home::classify_all(
            &home,
            Some(OsStr::new("/synthetic/claude-elsewhere")),
            Some(OsStr::new("relative/codex")),
            None,
        );
        let manifest = fs::read(brain_home.join("install-manifest.json")).unwrap();
        assert_eq!(
            uninstall_with(&home, &brain_home, false, true, &relocated).unwrap(),
            2
        );
        assert_eq!(
            fs::read(brain_home.join("install-manifest.json")).unwrap(),
            manifest,
            "partial uninstall keeps the manifest byte-identical"
        );
        (root, home, brain_home)
    }

    fn all_client_files() -> Vec<&'static str> {
        CLAUDE_FILES
            .iter()
            .chain(&CODEX_FILES)
            .copied()
            .chain([".cursor/mcp_config.json", ".cursor/rules/brain-for-ai.mdc"])
            .collect()
    }

    #[test]
    fn second_uninstall_after_a_partial_one_completes_and_cleans_up() {
        let (root, home, brain_home, binary) = guard_fixture("partial-full");
        fs::create_dir_all(home.join(".cursor")).unwrap();
        fs::write(home.join(".cursor/mcp_config.json"), CURSOR_BEFORE).unwrap();
        let original = snapshot(&home, &all_client_files());
        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).unwrap();
        let manifest_path = brain_home.join("install-manifest.json");
        let entries = serde_json::from_slice::<InstallManifest>(&fs::read(&manifest_path).unwrap())
            .unwrap()
            .entries
            .len();
        let relocated = crate::client_home::classify_all(
            &home,
            Some(OsStr::new("/synthetic/claude-elsewhere")),
            Some(OsStr::new("relative/codex")),
            None,
        );
        assert_eq!(
            uninstall_with(&home, &brain_home, false, true, &relocated).unwrap(),
            2
        );
        let state = brain_state(&brain_home);
        let dry = uninstall_with(&home, &brain_home, true, true, &unset_homes()).unwrap();
        assert_eq!(dry, entries - 2, "dry run counts only future restores");
        assert_eq!(brain_state(&brain_home), state, "dry run writes nothing");
        let second = uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap();
        assert_eq!(
            second,
            entries - 2,
            "already-restored Cursor entries are not restored again"
        );
        assert_eq!(
            snapshot(&home, &all_client_files()),
            original,
            "pre-install bytes everywhere"
        );
        assert!(!manifest_path.exists(), "manifest removed");
        assert!(
            !brain_home.join("install-backups").exists(),
            "backups removed"
        );
        assert!(
            !stable_copy_path(&brain_home).exists(),
            "stable binary removed"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn drifted_or_unreadable_entries_still_block_cleanup_after_a_partial_uninstall() {
        let (root, home, brain_home) = partially_uninstalled("partial-drift");
        let edited = b"{\"user\":\"edit\"}\n";
        fs::write(home.join(".cursor/mcp_config.json"), edited).unwrap();
        // A file Brain created, now unreadable (a directory): never absence.
        fs::remove_file(home.join(".codex/AGENTS.md")).unwrap();
        fs::create_dir(home.join(".codex/AGENTS.md")).unwrap();
        let manifest_path = brain_home.join("install-manifest.json");
        let manifest = fs::read(&manifest_path).unwrap();
        let backups = brain_state(&brain_home)
            .into_iter()
            .filter(|(path, _)| path.starts_with(brain_home.join("install-backups")))
            .collect::<Vec<_>>();
        assert!(!backups.is_empty());
        uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap();
        assert_eq!(
            fs::read(home.join(".cursor/mcp_config.json")).unwrap(),
            edited,
            "drift kept"
        );
        assert!(
            home.join(".codex/AGENTS.md").is_dir(),
            "unreadable entry not removed"
        );
        assert_eq!(
            fs::read(&manifest_path).unwrap(),
            manifest,
            "manifest kept byte-identical"
        );
        let kept = brain_state(&brain_home);
        for backup in &backups {
            assert!(kept.contains(backup), "{:?} kept", backup.0);
        }
        assert!(stable_copy_path(&brain_home).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_backup_aborts_before_any_file_changes() {
        let (root, home, brain_home) = partially_uninstalled("partial-tamper");
        for (path, _) in brain_state(&brain_home) {
            if path.starts_with(brain_home.join("install-backups")) {
                fs::write(path, b"tampered").unwrap();
            }
        }
        let files = all_client_files();
        let before = snapshot(&home, &files);
        let manifest = fs::read(brain_home.join("install-manifest.json")).unwrap();
        let error = uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap_err();
        assert!(
            error.to_string().contains("backup checksum mismatch"),
            "{error}"
        );
        assert_eq!(
            snapshot(&home, &files),
            before,
            "nothing changed before verification"
        );
        assert_eq!(
            fs::read(brain_home.join("install-manifest.json")).unwrap(),
            manifest
        );
        assert!(stable_copy_path(&brain_home).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn desktop_owner_keeps_the_binary_after_a_completed_partial_uninstall() {
        let (root, home, brain_home) = partially_uninstalled("partial-desktop");
        fs::write(desktop_copy_owner_path(&brain_home), b"desktop\n").unwrap();
        uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap();
        assert!(!brain_home.join("install-manifest.json").exists());
        assert!(
            stable_copy_path(&brain_home).exists(),
            "desktop shortcut still uses it"
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Rewrites the installed manifest (owner-only, as Brain writes it).
    fn rewrite_manifest(brain_home: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = brain_home.join("install-manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut value);
        crate::private_fs::write_private(&path, &serde_json::to_vec_pretty(&value).unwrap())
            .unwrap();
    }

    type Everything = (Vec<Option<Vec<u8>>>, Vec<(PathBuf, Vec<u8>)>);

    fn everything(home: &Path, brain_home: &Path) -> Everything {
        (snapshot(home, &all_client_files()), brain_state(brain_home))
    }

    fn assert_refused_unchanged(home: &Path, brain_home: &Path, label: &str) {
        let before = everything(home, brain_home);
        for dry_run in [true, false] {
            let error = uninstall_with(home, brain_home, dry_run, true, &unset_homes())
                .expect_err(label)
                .to_string();
            assert!(error.contains("refusing to uninstall"), "{label}: {error}");
            assert_eq!(
                everything(home, brain_home),
                before,
                "{label}: no partial change"
            );
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn manifest_naming_a_victim_with_a_valid_sha_is_refused_before_any_read() {
        let (root, home, brain_home) = installed_fixture("i2-victim");
        let victim = home.join("victim.txt");
        fs::write(&victim, b"precious\n").unwrap();
        let victim_sha = sha256(b"precious\n");
        for path in [
            victim.clone(),
            root.join("outside"),
            home.join(".codex/../victim.txt"),
        ] {
            let (path, sha) = (path.to_string_lossy().into_owned(), victim_sha.clone());
            rewrite_manifest(&brain_home, |m| {
                m["entries"][0]["path"] = path.clone().into();
                m["entries"][0]["existed"] = false.into();
                m["entries"][0]["before_sha256"] = serde_json::Value::Null;
                m["entries"][0]["backup"] = serde_json::Value::Null;
                m["entries"][0]["after_sha256"] = sha.into();
            });
            assert_refused_unchanged(&home, &brain_home, &path);
            assert_eq!(fs::read(&victim).unwrap(), b"precious\n");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_backups_hashes_and_schema_are_refused_without_changes() {
        let (root, home, brain_home) = installed_fixture("i2-schema");
        let original = fs::read(brain_home.join("install-manifest.json")).unwrap();
        let existed = serde_json::from_slice::<InstallManifest>(&original)
            .unwrap()
            .entries
            .iter()
            .position(|entry| entry.existed)
            .expect("an entry with a backup");
        type Edit = Box<dyn Fn(&mut serde_json::Value)>;
        let backup = |name: &'static str| -> Edit {
            Box::new(move |m: &mut serde_json::Value| m["entries"][existed]["backup"] = name.into())
        };
        let mut cases: Vec<(&str, Edit)> = vec![
            ("traversal backup", backup("../install-manifest.json")),
            ("absolute backup", backup("/etc/hostname")),
            ("nested backup", backup("001.bak/x")),
            ("short backup", backup("1.bak")),
            ("out-of-range backup", backup("999.bak")),
            ("other extension", backup("001.txt")),
            ("version", Box::new(|m| m["version"] = 2.into())),
            ("unknown field", Box::new(|m| m["extra"] = true.into())),
            (
                "unknown entry field",
                Box::new(move |m| m["entries"][existed]["client"] = "x".into()),
            ),
            (
                "bad sha",
                Box::new(move |m| m["entries"][existed]["after_sha256"] = "zz".into()),
            ),
            (
                "upper sha",
                Box::new(move |m| {
                    let upper = m["entries"][existed]["after_sha256"]
                        .as_str()
                        .unwrap()
                        .to_uppercase();
                    m["entries"][existed]["after_sha256"] = upper.into();
                }),
            ),
            (
                "bad before sha",
                Box::new(move |m| m["entries"][existed]["before_sha256"] = "00".into()),
            ),
            (
                "existed without backup",
                Box::new(move |m| m["entries"][existed]["backup"] = serde_json::Value::Null),
            ),
            (
                "setuid mode",
                Box::new(move |m| m["entries"][existed]["mode"] = 0o104755.into()),
            ),
            (
                "duplicate path",
                Box::new(|m| {
                    let first = m["entries"][0].clone();
                    m["entries"].as_array_mut().unwrap().push(first);
                }),
            ),
        ];
        cases.push((
            "duplicate backup",
            Box::new(move |m| {
                let entries = m["entries"].as_array_mut().unwrap();
                let name = entries[existed]["backup"].clone();
                let other = entries
                    .iter()
                    .position(|e| e["existed"] == true && e["backup"] != name);
                if let Some(other) = other {
                    entries[other]["backup"] = name;
                } else {
                    entries[existed]["path"] =
                        entries[(existed + 1) % entries.len()]["path"].clone();
                }
            }),
        ));
        for (label, edit) in cases {
            crate::private_fs::write_private(&brain_home.join("install-manifest.json"), &original)
                .unwrap();
            rewrite_manifest(&brain_home, |m| edit(m));
            assert_refused_unchanged(&home, &brain_home, label);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_manifest_and_backup_files_are_refused_without_changes() {
        let (root, home, brain_home) = installed_fixture("i2-files");
        let manifest = brain_home.join("install-manifest.json");
        let original = fs::read(&manifest).unwrap();
        let restore = || crate::private_fs::write_private(&manifest, &original).unwrap();

        set_mode(&manifest, 0o644);
        assert_refused_unchanged(&home, &brain_home, "permissive manifest");
        restore();

        let elsewhere = root.join("manifest-copy.json");
        crate::private_fs::write_private(&elsewhere, &original).unwrap();
        fs::remove_file(&manifest).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &manifest).unwrap();
        assert_refused_unchanged(&home, &brain_home, "symlinked manifest");
        fs::remove_file(&manifest).unwrap();
        restore();

        let mut padded = original.clone();
        padded.resize(MAX_MANIFEST_BYTES as usize + 1, b' ');
        crate::private_fs::write_private(&manifest, &padded).unwrap();
        assert_refused_unchanged(&home, &brain_home, "oversize manifest");
        restore();

        fs::remove_file(&manifest).unwrap();
        fs::create_dir(&manifest).unwrap();
        assert_refused_unchanged(&home, &brain_home, "manifest directory");
        fs::remove_dir(&manifest).unwrap();
        restore();

        set_mode(&brain_home, 0o755);
        assert_refused_unchanged(&home, &brain_home, "permissive Brain home");
        set_mode(&brain_home, 0o700);

        let backups = brain_home.join("install-backups");
        set_mode(&backups, 0o755);
        assert_refused_unchanged(&home, &brain_home, "permissive backup directory");
        set_mode(&backups, 0o700);

        let moved = root.join("backups-elsewhere");
        fs::rename(&backups, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &backups).unwrap();
        assert_refused_unchanged(&home, &brain_home, "symlinked backup directory");
        fs::remove_file(&backups).unwrap();
        fs::rename(&moved, &backups).unwrap();

        // Owned by someone else: possible to stage only as root.
        if crate::private_fs::current_uid() == Some(0) {
            std::os::unix::fs::chown(&manifest, Some(12345), None).unwrap();
            assert_refused_unchanged(&home, &brain_home, "foreign manifest owner");
            std::os::unix::fs::chown(&manifest, Some(0), None).unwrap();
        }

        // A permissive or symlinked backup file is refused at its read,
        // before any file is changed.
        let backup = fs::read_dir(&backups)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        set_mode(&backup, 0o644);
        let before = everything(&home, &brain_home);
        let error = uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap_err();
        assert!(error.to_string().contains("installation backup"), "{error}");
        assert_eq!(everything(&home, &brain_home), before);
        set_mode(&backup, 0o600);
        let bytes = fs::read(&backup).unwrap();
        let target = root.join("backup-target");
        crate::private_fs::write_private(&target, &bytes).unwrap();
        fs::remove_file(&backup).unwrap();
        std::os::unix::fs::symlink(&target, &backup).unwrap();
        let before = everything(&home, &brain_home);
        assert!(uninstall_with(&home, &brain_home, false, true, &unset_homes()).is_err());
        assert_eq!(everything(&home, &brain_home), before);
        fs::remove_file(&backup).unwrap();
        crate::private_fs::write_private(&backup, &bytes).unwrap();

        // The untouched canonical installation still uninstalls completely.
        assert!(uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap() > 0);
        assert!(!manifest.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_symlink_in_a_client_path_is_a_conflict_and_its_target_is_untouched() {
        let (root, home, brain_home) = installed_fixture("i2-parent");
        let real = root.join("real-codex");
        fs::rename(home.join(".codex"), &real).unwrap();
        std::os::unix::fs::symlink(&real, home.join(".codex")).unwrap();
        let before = snapshot(&real, &["config.toml", "hooks.json", "AGENTS.md"]);
        let manifest = fs::read(brain_home.join("install-manifest.json")).unwrap();
        uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap();
        assert_eq!(
            snapshot(&real, &["config.toml", "hooks.json", "AGENTS.md"]),
            before
        );
        assert_eq!(
            fs::read(brain_home.join("install-manifest.json")).unwrap(),
            manifest
        );
        assert_eq!(
            fs::read(home.join(".cursor/mcp_config.json")).unwrap(),
            CURSOR_BEFORE
        );
        assert!(stable_copy_path(&brain_home).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_backup_directory_is_fine_only_when_no_backup_is_needed() {
        let (root, home, brain_home) = installed_fixture("i2-nobackups");
        let backups = brain_home.join("install-backups");
        let moved = root.join("kept-backups");
        fs::rename(&backups, &moved).unwrap();
        let before = everything(&home, &brain_home);
        let error = uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap_err();
        assert!(error.to_string().contains("backup"), "{error}");
        assert_eq!(everything(&home, &brain_home), before, "no partial change");
        let _ = fs::remove_dir_all(&root);

        // Only created files: no backup is needed, so none must exist.
        let root = compat_root("i2-created-only");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".cursor")).unwrap();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("brain"), b"#!/bin/sh\nexit 0\n").unwrap();
        install_with(
            &home,
            &brain_home,
            &build.join("brain"),
            false,
            true,
            &unset_homes(),
        )
        .unwrap();
        let _ = fs::remove_dir_all(brain_home.join("install-backups"));
        assert!(uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap() > 0);
        assert!(!brain_home.join("install-manifest.json").exists());
        assert!(!home.join(".cursor/mcp_config.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Backup names come from the index of a plan among all plans, so an
    /// approved subset has gaps and does not match the entry ordinal.
    #[test]
    fn partial_selected_install_with_backup_index_gaps_uninstalls_completely() {
        let root = compat_root("i2-gaps");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let stable = stable_copy_path(&brain_home);
        // Plan 0 is already up to date, so it is not selected.
        let claude_json = format!(
            "{{\n  \"mcpServers\": {{\"brain\": {{\"command\": {}, \"args\": [\"--agent\", \"claude-code\", \"serve\"]}}}}\n}}\n",
            serde_json::to_string(&stable.to_string_lossy()).unwrap()
        );
        fs::write(home.join(".claude.json"), &claude_json).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            b"{\n  \"theme\": \"dark\"\n}\n",
        )
        .unwrap();
        let files = [".claude.json", ".claude/settings.json", ".claude/CLAUDE.md"];
        let original = snapshot(&home, &files);
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        fs::write(build.join("brain"), b"#!/bin/sh\nexit 0\n").unwrap();
        install_with(
            &home,
            &brain_home,
            &build.join("brain"),
            false,
            true,
            &unset_homes(),
        )
        .unwrap();
        let manifest: InstallManifest =
            serde_json::from_slice(&fs::read(brain_home.join("install-manifest.json")).unwrap())
                .unwrap();
        assert!(
            !manifest
                .entries
                .iter()
                .any(|entry| entry.path == home.join(".claude.json"))
        );
        assert_eq!(
            manifest.entries[0].backup.as_deref(),
            Some(Path::new("001.bak")),
            "gap at 000"
        );
        assert!(uninstall_with(&home, &brain_home, true, true, &unset_homes()).unwrap() > 0);
        assert!(uninstall_with(&home, &brain_home, false, true, &unset_homes()).unwrap() > 0);
        assert_eq!(snapshot(&home, &files), original);
        assert!(!brain_home.join("install-manifest.json").exists());
        assert!(!brain_home.join("install-backups").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_brain_home_or_manifest_stays_a_no_op() {
        let root = compat_root("i2-absent");
        let home = root.join("home");
        assert_eq!(
            uninstall_with(&home, &home.join(".brain"), false, true, &unset_homes()).unwrap(),
            0
        );
        fs::create_dir_all(&home).unwrap();
        crate::private_fs::ensure_private_home(&home.join(".brain")).unwrap();
        assert_eq!(
            uninstall_with(&home, &home.join(".brain"), false, true, &unset_homes()).unwrap(),
            0
        );
        assert!(!home.join(".brain/install-manifest.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Clients that would be planned, without Cursor, for compact assertions.
    fn build_plans_for(home: &Path, homes: &ClientHomes) -> Vec<AgentClient> {
        detect_clients(home, homes)
            .into_iter()
            .filter(|client| *client != AgentClient::Cursor)
            .collect()
    }

    fn compat_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "brain-compat-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    /// Nested `projects.*.mcpServers`, a decoy string and a comment come
    /// before the real user-scoped root entry.
    const NESTED_BEFORE_ROOT: &str = r#"{
  "projects": {
    "/work/a": {"mcpServers": {"other": {"command": "x"}}, "deep": {"a": [{"b": {"mcpServers": {"brain": 1}}}]}}
  },
  "note": "\"mcpServers\": {\"brain\": /opt/brain serve",
  // user comment
  "mcpServers": {
    "zeta": {"command": "z"},
    "brain": {"command": "/opt/brain", "args": ["--agent", "claude-code", "serve"]}
  }
}
"#;

    #[test]
    fn root_brain_after_nested_projects_is_found_and_left_byte_identical() {
        assert_eq!(
            patch_mcp_json(NESTED_BEFORE_ROOT, "/opt/brain").as_deref(),
            Ok(NESTED_BEFORE_ROOT)
        );
        let plain =
            NESTED_BEFORE_ROOT.replace(r#"["--agent", "claude-code", "serve"]"#, r#"["serve"]"#);
        assert_eq!(
            patch_mcp_json(&plain, "/opt/brain").as_deref(),
            Ok(plain.as_str())
        );
    }

    #[test]
    fn nested_project_servers_never_receive_the_user_entry() {
        let text = "{\n  \"projects\": {\"/w\": {\"mcpServers\": {\"brain\": {\"command\": \"/elsewhere\"}}}},\n  \"theme\": \"dark\"\n}\n";
        let after = patch_mcp_json(text, "/opt/brain").expect("root insert");
        let before: serde_json::Value = serde_json::from_str(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(
            parsed["projects"], before["projects"],
            "nested object untouched"
        );
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["mcpServers"]["brain"]["command"], "/opt/brain");
        assert_eq!(
            parsed["mcpServers"]["brain"]["args"],
            serde_json::json!(["serve"])
        );
        assert!(
            after.contains(
                r#""projects": {"/w": {"mcpServers": {"brain": {"command": "/elsewhere"}}}}"#
            ),
            "nested bytes kept verbatim"
        );
        assert_eq!(
            patch_mcp_json(&after, "/opt/brain").unwrap(),
            after,
            "repeat is a no-op"
        );
    }

    #[test]
    fn incompatible_or_unparseable_entries_fail_closed() {
        for brain in [
            r#"{"command": "/opt/brain-evil", "args": ["serve"]}"#,
            r#"{"command": "/usr/bin/other", "args": ["serve"], "note": "/opt/brain"}"#,
            r#"{"command": "/opt/brain", "args": ["serve", "--unsafe"]}"#,
            r#"{"command": "/opt/brain", "args": ["--agent", "--x", "serve"]}"#,
            r#"{"command": "/opt/brain", "args": ["serve"], "env": {"A": "1"}}"#,
            r#"{"type": "http", "command": "/opt/brain", "args": ["serve"]}"#,
            r#""/opt/brain serve""#,
        ] {
            let text = format!("{{\"mcpServers\": {{\"brain\": {brain}}}}}\n");
            assert!(patch_mcp_json(&text, "/opt/brain").is_err(), "{brain}");
        }
        for text in [
            "{\"mcpServers\": []}",
            "[1]",
            "{\"a\": 1,}",
            "{\"hooks\": 3}",
        ] {
            assert!(
                patch_mcp_json(text, "/opt/brain").is_err() || text.contains("hooks"),
                "{text}"
            );
            assert!(
                patch_hooks_json(text, "/opt/brain", "claude").is_err()
                    || text.contains("mcpServers"),
                "{text}"
            );
        }

        let root = compat_root("refuse");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        let bytes = b"{\"mcpServers\": {\"brain\": {\"command\": \"/usr/bin/other\", \"note\": \"/opt/brain serve\"}}}\n";
        fs::write(home.join(".claude.json"), bytes).unwrap();
        let plans = build_plans(&home, Path::new("/opt/brain"), &[AgentClient::Claude]);
        let plan = plans
            .iter()
            .find(|plan| plan.path == home.join(".claude.json"))
            .unwrap();
        assert!(plan.conflict.is_some());
        assert_eq!(plan.after.as_deref(), Some(bytes.as_slice()));
        assert_eq!(fs::read(home.join(".claude.json")).unwrap(), bytes);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hooks_are_matched_on_the_exact_command_field() {
        let ours = format!(
            "{} hook --event SessionStart --client claude",
            shell_quote("/opt/brain")
        );
        let decoy = serde_json::json!({
            "projects": {"/w": {"hooks": {}}},
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "logger", "note": ours}]}]}
        });
        let text = serde_json::to_string_pretty(&decoy).unwrap();
        let after = patch_hooks_json(&text, "/opt/brain", "claude").expect("patch");
        let parsed: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(
            parsed["hooks"]["SessionStart"].as_array().unwrap().len(),
            2,
            "decoy kept, ours added"
        );
        assert!(hook_registered(&parsed["hooks"]["SessionStart"], &ours));
        assert_eq!(parsed["projects"], decoy["projects"]);
        assert_eq!(
            patch_hooks_json(&after, "/opt/brain", "claude").unwrap(),
            after
        );
    }

    #[test]
    fn unmarked_codex_registrations_are_never_duplicated() {
        for existing in [
            "[mcp_servers.brain]\ncommand = \"/opt/brain\"\nargs = [\"serve\"]\n",
            "[mcp_servers]\nbrain = { command = \"/opt/brain\" }\n",
            "[ hooks . \"brain\" ]\nconfig = \"x\"\n",
            "[hooks.brain] # trailing comment\nconfig = \"x\"\n",
            "mcp_servers.brain.command = \"x\"\n",
            "mcp_servers = { other = { command = \"y\" } }\n",
            "[[mcp_servers]]\nname = \"y\"\n",
            "[mcp_servers.'brain'.env]\nA = \"1\"\n",
        ] {
            assert!(codex_config_conflicts(existing).is_err(), "{existing}");
        }
        for fine in [
            "# [mcp_servers.brain]\nmodel = \"gpt\"\n",
            "note = \"[mcp_servers.brain]\"\n[mcp_servers.other]\ncommand = \"y\"\n",
        ] {
            assert!(codex_config_conflicts(fine).is_ok(), "{fine}");
        }

        let root = compat_root("toml");
        let home = root.join("home");
        fs::create_dir_all(home.join(".codex")).unwrap();
        let bytes =
            b"# mine\n[mcp_servers.brain]\ncommand = \"/somewhere/brain\"\nargs = [\"serve\"]\n";
        fs::write(home.join(".codex/config.toml"), bytes).unwrap();
        let plans = build_plans(&home, Path::new("/opt/brain"), &[AgentClient::Codex]);
        let plan = plans
            .iter()
            .find(|plan| plan.path == home.join(".codex/config.toml"))
            .unwrap();
        assert!(
            plan.conflict
                .as_deref()
                .is_some_and(|text| text.contains("mcp_servers.brain"))
        );
        assert_eq!(plan.after.as_deref(), Some(bytes.as_slice()));
        // A file Brain already owns (marker block present, nothing unmarked) stays planned.
        let owned = upsert_marker(
            "model = \"gpt\"\n",
            "# BRAIN_FOR_AI_BEGIN\n[mcp_servers.brain]\n# BRAIN_FOR_AI_END\n",
            CONFIG_BEGIN,
            CONFIG_END,
        )
        .unwrap();
        assert!(codex_config_conflicts(&owned).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn escaped_or_ambiguous_toml_keys_fail_closed() {
        for existing in [
            "[mcp_servers.\"bra\\u0069n\"]\ncommand = \"x\"\n",
            "[mcp_servers.\"bra\\U00000069n\"]\ncommand = \"x\"\n",
            "[hooks.\"\\x62rain\"]\nconfig = \"x\"\n",
            "\"mcp_servers\".\"b\\u0072ain\".command = \"x\"\n",
            "[mcp_servers]\n\"br\\u0061in\" = { command = \"x\" }\n",
            "[mcp_servers.\"br\\qain\"]\n",
            "[mcp_servers.brain]] \n",
            "[mcp_servers.other] trailing\n",
            "model = \"gpt\"\nthis is not toml\n",
            "args = [\n  \"x\",\n",
        ] {
            assert!(codex_config_conflicts(existing).is_err(), "{existing}");
        }
        for fine in [
            "[mcp_servers.\" brain\"]\ncommand = \"x\"\n",
            "[mcp_servers.\"brains\"]\ncommand = \"x\"\n",
            "[projects.\"/home/u/x\"]\ntrust_level = \"trusted\"\n",
            "args = [\n  \"--flag=x\",\n  { brain = 1 },\n]\nmodel = \"gpt\" # note = 1\n",
        ] {
            assert!(codex_config_conflicts(fine).is_ok(), "{fine}");
        }
    }

    #[test]
    fn marker_text_cannot_hide_an_unmarked_registration() {
        for existing in [
            "a = \"# BRAIN_FOR_AI_BEGIN\"\n[mcp_servers.brain]\ncommand = \"x\"\nb = \"# BRAIN_FOR_AI_END\"\n",
            "# BRAIN_FOR_AI_BEGIN trailing\n[mcp_servers.brain]\n# BRAIN_FOR_AI_END\n",
            "text = \"\"\"\n# BRAIN_FOR_AI_BEGIN\n\"\"\"\n[mcp_servers.brain]\n",
            "# BRAIN_FOR_AI_BEGIN\n# BRAIN_FOR_AI_END\n# BRAIN_FOR_AI_BEGIN\n# BRAIN_FOR_AI_END\n",
            "# BRAIN_FOR_AI_END\n",
            "# BRAIN_FOR_AI_BEGIN\n[mcp_servers.brain]\n",
        ] {
            assert!(codex_config_conflicts(existing).is_err(), "{existing}");
        }
    }

    #[test]
    fn duplicate_json_keys_cannot_hide_an_incompatible_effective_entry() {
        let compatible = r#"{"command": "/opt/brain", "args": ["serve"]}"#;
        let evil = r#"{"command": "/tmp/evil", "args": ["serve"]}"#;
        for text in [
            format!("{{\"mcpServers\": {{\"brain\": {compatible}, \"brain\": {evil}}}}}"),
            format!(
                "{{\"mcpServers\": {{\"brain\": {compatible}}}, \"mcpServers\": {{\"brain\": {evil}}}}}"
            ),
            format!("{{\"mcpServers\": {{\"brain\": {compatible}, \"\\u0062rain\": {evil}}}}}"),
            "{\"mcpServers\": {\"brain\": {\"command\": \"/opt/brain\", \"args\": [\"serve\"], \"command\": \"/tmp/evil\"}}}"
                .to_owned(),
        ] {
            assert!(patch_mcp_json(&text, "/opt/brain").is_err(), "{text}");
        }
        assert!(
            patch_hooks_json("{\"hooks\": {}, \"hooks\": {}}", "/opt/brain", "claude").is_err()
        );
        let commented = "// a stray { in a comment\n{\"theme\": \"dark\"}\n";
        let after = patch_mcp_json(commented, "/opt/brain").expect("comment before root");
        assert!(after.starts_with("// a stray { in a comment\n{"));

        let root = compat_root("dup");
        let home = root.join("home");
        fs::create_dir_all(home.join(".codex")).unwrap();
        let json = format!("{{\"mcpServers\": {{\"brain\": {compatible}, \"brain\": {evil}}}}}\n");
        fs::write(home.join(".claude.json"), &json).unwrap();
        let toml = b"[mcp_servers.\"bra\\u0069n\"]\ncommand = \"/somewhere\"\n";
        fs::write(home.join(".codex/config.toml"), toml).unwrap();
        let plans = build_plans(
            &home,
            Path::new("/opt/brain"),
            &[AgentClient::Claude, AgentClient::Codex],
        );
        for (path, bytes) in [
            (home.join(".claude.json"), json.as_bytes()),
            (home.join(".codex/config.toml"), toml.as_slice()),
        ] {
            let plan = plans.iter().find(|plan| plan.path == path).unwrap();
            assert!(plan.conflict.is_some(), "{}", path.display());
            assert_eq!(plan.after.as_deref(), Some(bytes));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repeated_install_never_touches_refused_files() {
        let root = compat_root("refused-install");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".codex")).unwrap();
        let toml = b"# mine\n[mcp_servers.\"bra\\u0069n\"]\ncommand = \"/somewhere\"\n";
        fs::write(home.join(".codex/config.toml"), toml).unwrap();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let binary = build.join("brain");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        install_with(&home, &brain_home, &binary, true, true, &unset_homes()).expect("dry run");
        assert_eq!(fs::read(home.join(".codex/config.toml")).unwrap(), toml);
        assert!(
            !home.join(".codex/hooks.json").exists(),
            "dry run writes nothing"
        );
        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).expect("install");
        let first = (
            fs::read(home.join(".codex/hooks.json")).ok(),
            fs::read(home.join(".codex/AGENTS.md")).ok(),
        );
        assert_eq!(fs::read(home.join(".codex/config.toml")).unwrap(), toml);
        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).expect("repeat");
        assert_eq!(fs::read(home.join(".codex/config.toml")).unwrap(), toml);
        assert_eq!(
            (
                fs::read(home.join(".codex/hooks.json")).ok(),
                fs::read(home.join(".codex/AGENTS.md")).ok(),
            ),
            first
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn triple_quoted_values_fail_closed_and_cannot_hide_headers() {
        // Valid TOML (tomllib: mcp_servers.brain exists); a delimiter counter
        // that also counted the comment's `"""` skipped the header.
        let exact = "note = \"\"\"hello\"\"\" # \"\"\"\n[mcp_servers.brain]\ncommand = \"brain\"\n# \"\"\"\n";
        for existing in [
            exact,
            "note = \u{27}\u{27}\u{27}hello\u{27}\u{27}\u{27} # \u{27}\u{27}\u{27}\n[hooks.brain]\nconfig = \"x\"\n# \u{27}\u{27}\u{27}\n",
            "note = \"\"\"a\\\"\"\"\"\n[mcp_servers.other]\n",
            "note = \"\"\"\n# \"\"\"\n\"\"\"\n[mcp_servers.other]\n",
            "text = \"\"\"\n[mcp_servers.brain]\n\"\"\"\n",
            "plain = \"x\"\nnote = \u{27}\u{27}\u{27}single line\u{27}\u{27}\u{27}\n",
            "[mcp_servers.other]\nenv = \"\"\"\nmulti\n\"\"\"\n",
            "note = \"a \\\"\\\"\\\" b\"\n[mcp_servers.brain]\n",
            "# \"\"\"\n[mcp_servers.brain]\n# \"\"\"\n",
        ] {
            assert!(codex_config_conflicts(existing).is_err(), "{existing}");
        }
        for fine in [
            "note = \"hello # not a comment \\\"\\\"\\\"\"\n[mcp_servers.other]\n",
            "args = [\n  \"a\",\n  \"b\", # \"\"\"\n]\n",
            "x = \"\" # \"\"\"\nmodel = \"gpt\"\n",
        ] {
            assert!(codex_config_conflicts(fine).is_ok(), "{fine}");
        }

        let root = compat_root("triple");
        let home = root.join("home");
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(home.join(".codex/config.toml"), exact).unwrap();
        let plans = build_plans(&home, Path::new("/opt/brain"), &[AgentClient::Codex]);
        let plan = plans
            .iter()
            .find(|plan| plan.path == home.join(".codex/config.toml"))
            .unwrap();
        assert!(plan.conflict.is_some());
        assert_eq!(plan.after.as_deref(), Some(exact.as_bytes()));
        assert_eq!(
            fs::read(home.join(".codex/config.toml")).unwrap(),
            exact.as_bytes()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_writes_nothing_and_repeated_install_keeps_compatible_bytes() {
        let root = compat_root("install");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".codex")).unwrap();
        let stable = stable_copy_path(&brain_home);
        let claude_json = format!(
            "{{\n  \"projects\": {{\"/w\": {{\"mcpServers\": {{}}}}}},\n  \"mcpServers\": {{\"brain\": {{\"command\": {}, \"args\": [\"--agent\", \"claude-code\", \"serve\"]}}}}\n}}\n",
            serde_json::to_string(&stable.to_string_lossy()).unwrap()
        );
        fs::write(home.join(".claude.json"), &claude_json).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            "{\n  \"theme\": \"dark\"\n}\n",
        )
        .unwrap();
        fs::write(
            home.join(".codex/config.toml"),
            "# mine\n[mcp_servers.brain]\ncommand = \"/somewhere/brain\"\nargs = [\"serve\"]\n",
        )
        .unwrap();
        let build = root.join("build");
        fs::create_dir_all(&build).unwrap();
        let binary = build.join("brain");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").unwrap();
        let files = [
            home.join(".claude.json"),
            home.join(".claude/settings.json"),
            home.join(".claude/CLAUDE.md"),
            home.join(".codex/config.toml"),
            home.join(".codex/hooks.json"),
            home.join(".codex/AGENTS.md"),
        ];
        let snapshot = || {
            files
                .iter()
                .map(|path| fs::read(path).ok())
                .collect::<Vec<_>>()
        };
        let before = snapshot();

        install_with(&home, &brain_home, &binary, true, true, &unset_homes()).expect("dry run");
        assert_eq!(snapshot(), before, "dry run writes nothing");
        assert!(!brain_home.join("install-manifest.json").exists());
        assert!(!stable.exists());

        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).expect("install");
        let first = snapshot();
        assert_eq!(first[0], before[0], "compatible root entry byte-identical");
        assert_eq!(first[3], before[3], "conflicting TOML untouched");
        assert_ne!(first[1], before[1], "hooks were added");
        let settings: serde_json::Value =
            serde_json::from_slice(first[1].as_ref().unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");

        install_with(&home, &brain_home, &binary, false, true, &unset_homes()).expect("repeat");
        assert_eq!(snapshot(), first, "repeated install changes nothing");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn claude_user_config_is_planned_even_when_absent() {
        let home =
            std::env::temp_dir().join(format!("brain-missing-config-{}", std::process::id()));
        assert!(!home.exists());
        let plans = build_plans(&home, Path::new("/opt/brain"), &[AgentClient::Claude]);
        let plan = plans
            .iter()
            .find(|plan| plan.path == home.join(".claude.json"))
            .unwrap();
        assert!(plan.before.is_none());
        assert!(plan.conflict.is_none());
        let value: serde_json::Value =
            serde_json::from_slice(plan.after.as_ref().unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["brain"]["command"], "/opt/brain");
        assert!(!home.exists(), "planning must not create configuration");
    }

    #[test]
    fn unreadable_or_non_utf8_json_is_never_treated_as_missing() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("brain-invalid-config-{nonce}"));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        let bytes = [0xff, 0xfe, 0x7b];
        fs::write(&path, bytes).unwrap();
        let plan = json_plan(AgentClient::Claude, path.clone(), "test", |text| {
            patch_mcp_json(text, "/opt/brain")
        });
        assert!(plan.conflict.is_some());
        assert_eq!(plan.before, plan.after);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let unreadable = json_plan(AgentClient::Claude, root.clone(), "test", |text| {
            patch_mcp_json(text, "/opt/brain")
        });
        assert!(unreadable.conflict.is_some());
        assert!(unreadable.after.is_none());
        fs::remove_dir_all(root).unwrap();
    }
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn install_is_idempotent_and_uninstall_restores_exact_bytes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("brain-installer-{}-{nonce}", std::process::id()));
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".claude")).expect("claude dir");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        fs::create_dir_all(home.join(".cursor")).expect("cursor dir");
        let fixtures = [
            (
                home.join(".claude.json"),
                b"{\n  \"mcpServers\": {}, \"unrelated\": true\n}\n".as_slice(),
            ),
            (
                home.join(".claude/settings.json"),
                b"{\n  \"theme\": \"dark\"\n}\n".as_slice(),
            ),
            (
                home.join(".codex/config.toml"),
                b"# keep this comment\nmodel = \"gpt\"\n".as_slice(),
            ),
            (
                home.join(".cursor/mcp_config.json"),
                b"{\"mcpServers\":{}}\n".as_slice(),
            ),
        ];
        for (path, bytes) in &fixtures {
            fs::write(path, bytes).expect("write fixture");
        }

        let source_dir = root.join("build");
        fs::create_dir_all(&source_dir).expect("build dir");
        let binary = source_dir.join("brain");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n").expect("write binary");

        let first = install_with(&home, &brain_home, &binary, false, true, &unset_homes())
            .expect("install");
        assert!(first > 0);
        let user_config: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".claude.json")).unwrap()).unwrap();
        let hook_config: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".claude/settings.json")).unwrap()).unwrap();
        assert!(user_config.pointer("/mcpServers/brain").is_some());
        assert!(hook_config.get("mcpServers").is_none());
        let observed = crate::doctor::document_with(&home, &brain_home, 1, &unset_homes());
        assert_eq!(observed["clients"][0]["mcp_config_entry_present"], true);
        assert_eq!(observed["clients"][0]["hook_config_marker_present"], true);
        let copy = stable_copy_path(&brain_home);
        assert!(copy.exists(), "the binary must be copied into Brain's home");
        for (path, _) in &fixtures {
            let text = String::from_utf8(fs::read(path).expect("installed bytes")).expect("utf8");
            if text.contains("brain") {
                assert!(
                    !text.contains(&source_dir.display().to_string()),
                    "hooks must not point into the build directory: {text}"
                );
            }
        }
        let installed = fixtures
            .iter()
            .map(|(path, _)| fs::read(path).expect("installed bytes"))
            .collect::<Vec<_>>();
        let second = install_with(&home, &brain_home, &binary, false, true, &unset_homes())
            .expect("idempotent install");
        assert_eq!(second, 0);
        for ((path, _), expected) in fixtures.iter().zip(installed) {
            assert_eq!(fs::read(path).expect("second install bytes"), expected);
        }

        uninstall_with(&home, &brain_home, false, true, &unset_homes()).expect("uninstall");
        for (path, expected) in &fixtures {
            assert_eq!(fs::read(path).expect("restored bytes"), *expected);
        }
        assert!(!copy.exists(), "uninstall must take its own copy with it");
        fs::remove_dir_all(root).expect("remove fixture root");
    }

    #[cfg(unix)]
    #[test]
    fn removing_agent_hooks_keeps_a_desktop_owned_binary() {
        use std::os::unix::fs::PermissionsExt;

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-installer-shared-copy-{}-{nonce}",
            std::process::id()
        ));
        let home = root.join("home");
        let brain_home = home.join(".brain");
        fs::create_dir_all(home.join(".codex")).expect("codex dir");
        fs::write(home.join(".codex/config.toml"), b"model = \"gpt\"\n").expect("codex config");
        let source = root.join("target/release/brain");
        fs::create_dir_all(source.parent().expect("source parent")).expect("target dir");
        fs::write(&source, b"#!/bin/sh\nexit 0\n").expect("fake brain");
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).expect("source executable");

        install(&home, &brain_home, &source, false, true).expect("install agent hook");
        fs::write(
            desktop_copy_owner_path(&brain_home),
            b"brain-for-ai-settings-v1\n",
        )
        .expect("desktop owner marker");
        uninstall_with(&home, &brain_home, false, true, &unset_homes()).expect("remove agent hook");

        let stable = stable_copy_path(&brain_home);
        assert!(stable.is_file(), "desktop ownership must retain the binary");
        assert!(
            Command::new(&stable)
                .status()
                .expect("launch retained copy")
                .success()
        );
        fs::remove_dir_all(root).expect("remove shared-copy fixture");
    }

    #[test]
    fn sha256_matches_nist_vector() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn refuses_user_config_when_home_is_a_git_worktree() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-installer-git-{}-{nonce}",
            std::process::id()
        ));
        let home = root.join("home");
        fs::create_dir_all(home.join(".git")).expect("git metadata dir");
        fs::write(home.join(".git/HEAD"), b"ref: refs/heads/main\n").expect("git head");
        fs::create_dir_all(home.join(".claude")).expect("claude dir");
        let settings = home.join(".claude/settings.json");
        let original = b"{\"theme\":\"dark\"}\n";
        fs::write(&settings, original).expect("settings fixture");

        let result = install(
            &home,
            &home.join(".brain"),
            Path::new("/usr/local/bin/brain"),
            false,
            true,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&settings).expect("unchanged settings"), original);
        fs::remove_dir_all(root).expect("remove fixture root");
    }
}
