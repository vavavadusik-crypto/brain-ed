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
fn codex_config_conflicts(original: &str) -> Result<(), String> {
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
        install(&home, &brain_home, &binary, true, true).expect("dry run");
        assert_eq!(fs::read(home.join(".codex/config.toml")).unwrap(), toml);
        assert!(
            !home.join(".codex/hooks.json").exists(),
            "dry run writes nothing"
        );
        install(&home, &brain_home, &binary, false, true).expect("install");
        let first = (
            fs::read(home.join(".codex/hooks.json")).ok(),
            fs::read(home.join(".codex/AGENTS.md")).ok(),
        );
        assert_eq!(fs::read(home.join(".codex/config.toml")).unwrap(), toml);
        install(&home, &brain_home, &binary, false, true).expect("repeat");
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

        install(&home, &brain_home, &binary, true, true).expect("dry run");
        assert_eq!(snapshot(), before, "dry run writes nothing");
        assert!(!brain_home.join("install-manifest.json").exists());
        assert!(!stable.exists());

        install(&home, &brain_home, &binary, false, true).expect("install");
        let first = snapshot();
        assert_eq!(first[0], before[0], "compatible root entry byte-identical");
        assert_eq!(first[3], before[3], "conflicting TOML untouched");
        assert_ne!(first[1], before[1], "hooks were added");
        let settings: serde_json::Value =
            serde_json::from_slice(first[1].as_ref().unwrap()).unwrap();
        assert_eq!(settings["theme"], "dark");

        install(&home, &brain_home, &binary, false, true).expect("repeat");
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
