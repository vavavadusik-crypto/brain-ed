use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const MD_BEGIN: &str = "<!-- BRAIN_FOR_AI_BEGIN -->";
const MD_END: &str = "<!-- BRAIN_FOR_AI_END -->";
const CONFIG_BEGIN: &str = "# BRAIN_FOR_AI_BEGIN";
const CONFIG_END: &str = "# BRAIN_FOR_AI_END";
const DESKTOP_COPY_OWNER: &str = "desktop-shortcut-owner";

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
struct InstallManifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
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
    validate_versionless_binary(binary)?;
    let clients = detect_clients(home);
    if clients.is_empty() {
        println!("No supported agents detected; nothing to change.");
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
    brain_home: &Path,
    dry_run: bool,
    yes: bool,
) -> Result<usize, Box<dyn Error>> {
    let manifest_path = brain_home.join("install-manifest.json");
    if !manifest_path.exists() {
        println!("No Brain agent installation manifest found; nothing to remove.");
        return Ok(0);
    }
    let manifest: InstallManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let backup_dir = brain_home.join("install-backups");
    let mut restored = 0_usize;
    let mut skipped = false;
    for entry in &manifest.entries {
        let current = fs::read(&entry.path).ok();
        if current.as_deref().map(sha256).as_deref() != Some(entry.after_sha256.as_str()) {
            println!(
                "CONFLICT {}: file changed after installation; leaving it and its backup untouched",
                entry.path.display()
            );
            skipped = true;
            continue;
        }
        let original = entry
            .backup
            .as_ref()
            .map(|relative| fs::read(backup_dir.join(relative)))
            .transpose()?;
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
        if entry.existed {
            let bytes = original.as_deref().ok_or("missing installation backup")?;
            atomic_write(&entry.path, bytes, entry.mode)?;
            if entry.before_sha256.as_deref() != Some(sha256(bytes).as_str()) {
                return Err(
                    format!("backup checksum mismatch for {}", entry.path.display()).into(),
                );
            }
        } else if entry.path.exists() {
            fs::remove_file(&entry.path)?;
        }
        restored += 1;
    }
    if dry_run {
        println!(
            "Dry run: {} file(s) would be restored; no files written.",
            manifest.entries.len()
        );
        return Ok(0);
    }
    if !skipped && restored == manifest.entries.len() {
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

fn detect_clients(home: &Path) -> Vec<AgentClient> {
    let mut clients = Vec::new();
    if home.join(".claude").exists() || executable_on_path("claude") {
        clients.push(AgentClient::Claude);
    }
    if home.join(".codex").exists() || executable_on_path("codex") {
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

fn marker_plan(
    client: AgentClient,
    path: PathBuf,
    description: &str,
    block: &str,
    begin: &str,
    end: &str,
) -> PlannedChange {
    let before = fs::read(&path).ok();
    let original = before
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or_default();
    match upsert_marker(original, block, begin, end) {
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
    if let Some((start, end)) = find_property_value(&text, root, "mcpServers") {
        let servers = object_bounds(&text, start)?;
        if let Some((brain_start, brain_end)) = find_property_value(&text, servers, "brain") {
            let current = &text[brain_start..brain_end];
            if current.contains(&serde_json::to_string(binary).unwrap_or_default())
                && current.contains("\"serve\"")
            {
                return Ok(text);
            }
            return Err("mcpServers.brain already exists with different settings".to_owned());
        }
        text = insert_object_member(&text, servers, &entry);
        let _ = end;
    } else {
        let value = format!("\"mcpServers\": {{{entry}}}");
        text = insert_object_member(&text, root, &value);
    }
    Ok(text)
}

fn patch_hooks_json(original: &str, binary: &str, client: &str) -> Result<String, String> {
    let mut text = ensure_json_object(original)?;
    let hook_binary = shell_quote(binary);
    let root = object_bounds(&text, 0)?;
    if find_property_value(&text, root, "hooks").is_none() {
        text = insert_object_member(&text, root, "\"hooks\": {}");
    }
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "PreCompact",
        "Stop",
        "SessionEnd",
    ] {
        let root = object_bounds(&text, 0)?;
        let (hooks_start, _) = find_property_value(&text, root, "hooks")
            .ok_or_else(|| "hooks object insertion failed".to_owned())?;
        let hooks = object_bounds(&text, hooks_start)?;
        let command = format!("{hook_binary} hook --event {event} --client {client}");
        let item = format!(
            "{{\"hooks\":[{{\"type\":\"command\",\"command\":{}}}]}}",
            serde_json::to_string(&command).map_err(|error| error.to_string())?
        );
        if let Some((event_start, event_end)) = find_property_value(&text, hooks, event) {
            let current = &text[event_start..event_end];
            if current.contains(&serde_json::to_string(&command).unwrap_or_default()) {
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
    Ok(text)
}

fn ensure_json_object(original: &str) -> Result<String, String> {
    if original.trim().is_empty() {
        return Ok("{}\n".to_owned());
    }
    let text = original.to_owned();
    object_bounds(&text, 0)?;
    Ok(text)
}

fn object_bounds(text: &str, start_at: usize) -> Result<(usize, usize), String> {
    let start = text[start_at..]
        .find('{')
        .map(|offset| start_at + offset)
        .ok_or_else(|| "expected JSON object".to_owned())?;
    let end = matching_delimiter(text, start, b'{', b'}')?;
    Ok((start, end))
}

fn array_bounds(text: &str, start_at: usize) -> Result<(usize, usize), String> {
    let start = text[start_at..]
        .find('[')
        .map(|offset| start_at + offset)
        .ok_or_else(|| "expected JSON array".to_owned())?;
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

fn find_property_value(text: &str, bounds: (usize, usize), key: &str) -> Option<(usize, usize)> {
    let needle = format!("\"{key}\"");
    let relative = text[bounds.0 + 1..bounds.1].find(&needle)?;
    let key_start = bounds.0 + 1 + relative;
    let colon = text[key_start + needle.len()..bounds.1].find(':')? + key_start + needle.len();
    let start =
        text[colon + 1..bounds.1].find(|character: char| !character.is_whitespace())? + colon + 1;
    let end = match text.as_bytes().get(start) {
        Some(b'{') => matching_delimiter(text, start, b'{', b'}').ok()? + 1,
        Some(b'[') => matching_delimiter(text, start, b'[', b']').ok()? + 1,
        Some(b'"') => {
            let mut index = start + 1;
            let bytes = text.as_bytes();
            let mut escaped = false;
            while index < bounds.1 {
                if escaped {
                    escaped = false;
                } else if bytes[index] == b'\\' {
                    escaped = true;
                } else if bytes[index] == b'"' {
                    break;
                }
                index += 1;
            }
            index + 1
        }
        _ => text[start..bounds.1]
            .find([',', '}'])
            .map_or(bounds.1, |offset| start + offset),
    };
    Some((start, end))
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

fn sha256(bytes: &[u8]) -> String {
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

fn toml_string(value: &str) -> String {
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

        let first = install(&home, &brain_home, &binary, false, true).expect("install");
        assert!(first > 0);
        let user_config: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".claude.json")).unwrap()).unwrap();
        let hook_config: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join(".claude/settings.json")).unwrap()).unwrap();
        assert!(user_config.pointer("/mcpServers/brain").is_some());
        assert!(hook_config.get("mcpServers").is_none());
        let observed = crate::doctor::document(&home, &brain_home, 1);
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
        let second = install(&home, &brain_home, &binary, false, true).expect("idempotent install");
        assert_eq!(second, 0);
        for ((path, _), expected) in fixtures.iter().zip(installed) {
            assert_eq!(fs::read(path).expect("second install bytes"), expected);
        }

        uninstall(&brain_home, false, true).expect("uninstall");
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
        uninstall(&brain_home, false, true).expect("remove agent hook");

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
