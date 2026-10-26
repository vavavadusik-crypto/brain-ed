//! Fixed argv subscription adapters. CLI output is private, bounded, and never diagnostic text.
use crate::llm::LlmProvider;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LIMIT: usize = 4 * 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "brain-command-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => break,
            }
        }
        Err("command scratch unavailable".into())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[allow(unsafe_code)] // geteuid has no pointers or side effects.
fn executable(name: &str) -> Result<PathBuf, String> {
    if !["claude", "codex"].contains(&name) {
        return Err("unsupported command adapter".into());
    }
    let path = std::env::var_os("PATH").ok_or("command PATH unavailable")?;
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let trusted_owner = |meta: &fs::Metadata| meta.uid() == 0 || meta.uid() == uid;
    // Group write is harmless only for our own private group (Ubuntu's user
    // private groups, default umask 002): our gid with no listed members.
    let private_group = group_is_private(gid, uid);
    let writable_by_others = |meta: &fs::Metadata| {
        meta.mode() & 0o002 != 0
            || (meta.mode() & 0o020 != 0 && !(meta.gid() == gid && private_group))
    };
    for directory in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
        let candidate = directory.join(name);
        let Ok(link) = fs::symlink_metadata(&candidate) else {
            continue;
        };
        // Standard installs (Claude Code, Codex, npm) put a symlink in PATH, so
        // the link is resolved once and the resolved file is what runs: a later
        // swap of the link cannot change the executed program. The link, the
        // file and every directory above the file must be owned by root or by
        // us and not writable by group/other.
        let unsafe_error = || {
            "command executable unsafe (owned executable not writable by other users required)"
                .to_owned()
        };
        if !trusted_owner(&link) {
            return Err(unsafe_error());
        }
        let resolved = fs::canonicalize(&candidate).map_err(|_| unsafe_error())?;
        let meta = fs::metadata(&resolved).map_err(|_| unsafe_error())?;
        if !meta.is_file()
            || !trusted_owner(&meta)
            || writable_by_others(&meta)
            || meta.mode() & 0o111 == 0
        {
            return Err(unsafe_error());
        }
        for ancestor in resolved.ancestors().skip(1) {
            let dir = fs::metadata(ancestor).map_err(|_| unsafe_error())?;
            // A root-owned sticky directory (/tmp) is writable by all, but no
            // one can replace entries they do not own.
            let sticky_root = dir.uid() == 0 && dir.mode() & 0o1000 != 0;
            if !trusted_owner(&dir) || (writable_by_others(&dir) && !sticky_root) {
                return Err(unsafe_error());
            }
        }
        return Ok(resolved);
    }
    Err("command executable missing from PATH".into())
}
/// Codex's `--output-schema` is always OpenAI strict mode: every object needs
/// `additionalProperties: false` and lists all properties as required, so an
/// optional property becomes "this type or null".
fn openai_strict_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if key != "properties" && key != "required" {
                    out.insert(key.clone(), openai_strict_schema(value));
                }
            }
            if let Some(Value::Object(properties)) = map.get("properties") {
                let required: Vec<&str> = map
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut strict = serde_json::Map::new();
                for (name, property) in properties {
                    let mut property = openai_strict_schema(property);
                    if !required.contains(&name.as_str()) {
                        property = nullable(property);
                    }
                    strict.insert(name.clone(), property);
                }
                out.insert(
                    "required".into(),
                    Value::Array(properties.keys().cloned().map(Value::String).collect()),
                );
                out.insert("properties".into(), Value::Object(strict));
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(openai_strict_schema).collect()),
        other => other.clone(),
    }
}

fn nullable(mut property: Value) -> Value {
    if let Some(kind) = property.get("type").cloned() {
        let mut kinds = match kind {
            Value::Array(kinds) => kinds,
            single => vec![single],
        };
        if !kinds.contains(&json!("null")) {
            kinds.push(json!("null"));
        }
        property["type"] = Value::Array(kinds);
    }
    if let Some(Value::Array(options)) = property.get_mut("enum")
        && !options.contains(&Value::Null)
    {
        options.push(Value::Null);
    }
    property
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, without_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

/// True only for a user private group: `/etc/group` lists `gid` with no
/// supplementary members, and no account in `/etc/passwd` other than `uid`
/// has it as its primary group. Groups known only to NSS (LDAP, sssd) or
/// unreadable files count as shared, so group write is refused.
fn group_is_private(gid: u32, uid: u32) -> bool {
    let (Ok(groups), Ok(passwd)) = (
        fs::read_to_string("/etc/group"),
        fs::read_to_string("/etc/passwd"),
    ) else {
        return false;
    };
    group_is_private_in(&groups, &passwd, gid, uid)
}

fn group_is_private_in(groups: &str, passwd: &str, gid: u32, uid: u32) -> bool {
    let listed_without_members = groups.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        fields.len() == 4 && fields[2].parse::<u32>() == Ok(gid) && fields[3].trim().is_empty()
    });
    let other_primary_member = passwd.lines().any(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        fields.len() >= 4
            && fields[3].parse::<u32>() == Ok(gid)
            && fields[2].parse::<u32>() != Ok(uid)
    });
    listed_without_members && !other_primary_member
}

fn private_file(path: &std::path::Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "command private file unavailable".into())
}
fn reader(
    mut stream: impl Read + Send + 'static,
    exceeded: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut result = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if result.len() + n > LIMIT {
                        exceeded.store(true, Ordering::Relaxed);
                        break;
                    }
                    result.extend_from_slice(&buffer[..n]);
                }
            }
        }
        result
    })
}
#[allow(unsafe_code)] // kill targets only the process group created for our child.
fn kill_group(id: u32) {
    unsafe {
        libc::kill(-(id as i32), libc::SIGKILL);
    }
}
fn limited(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    [
        "usage limit",
        "rate limit",
        "rate_limit",
        "usage_limit",
        "quota exceeded",
        "limit reached",
        "hit your limit",
    ]
    .iter()
    .any(|s| text.contains(s))
}

pub(crate) fn run(
    provider: &LlmProvider,
    prompt: &str,
    schema: Option<&Value>,
    timeout: Duration,
) -> Result<String, String> {
    let binary = executable(&provider.command)?;
    let scratch = Scratch::new()?;
    let cwd = scratch.0.join("cwd");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&cwd)
        .map_err(|_| "command cwd unavailable")?;
    let schema = schema.cloned().unwrap_or_else(|| json!({"type":"object"}));
    let mut command = Command::new(binary);
    // The resolved file runs, but under its usual name, as from a PATH lookup.
    command.arg0(&provider.command);
    command.env_clear();
    // Exact allowlist: never copy arbitrary client-prefixed variables or API keys.
    for key in ["PATH", "HOME", "LANG", "CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let output_path = scratch.0.join("answer.json");
    let mut output_file = None;
    if provider.command == "claude" {
        command.args([
            "-p",
            "--output-format",
            "json",
            "--json-schema",
            &schema.to_string(),
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--no-session-persistence",
            // The distiller's own call must not run the user's hooks: they
            // would journal this session into the archive being distilled.
            // `--bare` would also do it but disables subscription (OAuth) login.
            "--setting-sources",
            "project",
            "--settings",
            "{\"disableAllHooks\":true}",
        ]);
        if !provider.model.is_empty() {
            command.args(["--model", &provider.model]);
        }
        if !provider.effort.is_empty() {
            command.args(["--effort", &provider.effort]);
        }
    } else {
        let schema_path = scratch.0.join("schema.json");
        private_file(&schema_path)?
            .write_all(openai_strict_schema(&schema).to_string().as_bytes())
            .map_err(|_| "command schema write failed")?;
        output_file = Some(private_file(&output_path)?);
        command
            .args([
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--output-schema",
            ])
            .arg(schema_path)
            .arg("-o")
            .arg(&output_path);
        if !provider.model.is_empty() {
            command.args(["-m", &provider.model]);
        }
        if !provider.effort.is_empty() {
            command.args(["-c", &format!("model_reasoning_effort={}", provider.effort)]);
        }
        command.arg("-");
    }
    command
        .current_dir(cwd)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| "command spawn failed")?;
    let started = Instant::now();
    let exceeded = Arc::new(AtomicBool::new(false));
    let out = reader(child.stdout.take().unwrap(), exceeded.clone());
    let err = reader(child.stderr.take().unwrap(), exceeded.clone());
    let mut stdin = child.stdin.take().unwrap();
    let prompt = prompt.to_owned();
    let input = std::thread::spawn(move || stdin.write_all(prompt.as_bytes()));
    let result: Result<std::process::ExitStatus, String> = loop {
        if exceeded.load(Ordering::Relaxed)
            || output_file.as_ref().is_some_and(|f| {
                f.metadata().map_or(true, |m| m.len() > LIMIT as u64)
                    || fs::symlink_metadata(&output_path)
                        .map_or(true, |m| !m.is_file() || m.len() > LIMIT as u64)
            })
        {
            break Err("command response too large".into());
        }
        if started.elapsed() >= timeout {
            break Err("command timed out".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(_) => break Err("command wait failed".into()),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Also clean descendants that outlive the parent and hold pipe descriptors.
    kill_group(child.id());
    let _ = child.wait();
    let _ = input.join();
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let status = result?;
    if exceeded.load(Ordering::Relaxed) {
        return Err("command response too large".into());
    }
    if !status.success() && (limited(&stdout) || limited(&stderr) || status.code() == Some(429)) {
        return Err("provider_limited".into());
    }
    if !status.success() {
        return Err(format!(
            "command failed (exit {})",
            status.code().map_or("signal".into(), |n| n.to_string())
        ));
    }
    if provider.command == "claude" {
        let parsed: Value =
            serde_json::from_slice(&stdout).map_err(|_| "command invalid JSON envelope")?;
        // Depending on version and settings (verbose), the CLI prints one result
        // object or the whole event list ending with `{"type":"result",...}`.
        let envelope = match parsed {
            Value::Array(events) => events
                .into_iter()
                .rev()
                .find(|event| event["type"] == "result")
                .ok_or("command missing result event")?,
            other => other,
        };
        if envelope["is_error"] == true {
            if limited(&stdout) {
                return Err("provider_limited".into());
            }
            return Err("command client error".into());
        }
        let document = envelope
            .get("structured_output")
            .filter(|v| v.is_object())
            .ok_or("command missing structured_output")?;
        Ok(document.to_string())
    } else {
        let mut bytes = Vec::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&output_path)
            .map_err(|_| "command result open failed")?
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "command result read failed")?;
        if bytes.len() > LIMIT {
            return Err("command response too large".into());
        }
        let text = String::from_utf8(bytes).map_err(|_| "command invalid UTF-8")?;
        // Strict mode answers optional fields with null; drop them so the
        // answer is checked against the original schema like any provider's.
        match serde_json::from_str::<Value>(&text) {
            Ok(document) => Ok(without_nulls(document).to_string()),
            Err(_) => Ok(text),
        }
    }
}

#[cfg(test)]
mod private_group_tests {
    use super::group_is_private_in;

    const PASSWD: &str = "root:x:0:0::/root:/bin/sh\nme:x:1000:1000::/home/me:/bin/sh\n";

    #[test]
    fn only_a_memberless_group_no_other_account_uses_is_private() {
        assert!(group_is_private_in("me:x:1000:\n", PASSWD, 1000, 1000));
        // Supplementary members listed in /etc/group.
        assert!(!group_is_private_in(
            "me:x:1000:guest\n",
            PASSWD,
            1000,
            1000
        ));
        // Another account has the group as its primary group.
        let shared = format!("{PASSWD}guest:x:1001:1000::/home/guest:/bin/sh\n");
        assert!(!group_is_private_in("me:x:1000:\n", &shared, 1000, 1000));
        // Known only to NSS (absent from /etc/group).
        assert!(!group_is_private_in("other:x:2000:\n", PASSWD, 1000, 1000));
    }
}

#[cfg(test)]
mod strict_schema_tests {
    use super::{openai_strict_schema, without_nulls};
    use serde_json::json;

    #[test]
    fn optional_properties_become_required_nullable_and_objects_closed() {
        let schema = json!({"type":"object","properties":{
            "facts":{"type":"array","items":{"type":"object","properties":{
                "what":{"type":"string"},
                "type":{"type":"string","enum":["error","plan"]},
                "links":{"type":"array","items":{"type":"object","properties":{"r":{"type":"string"}},"required":["r"]}}
            },"required":["what"]}}},"required":["facts"]});
        let strict = openai_strict_schema(&schema);
        assert_eq!(strict["additionalProperties"], false);
        let item = &strict["properties"]["facts"]["items"];
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["required"], json!(["links", "type", "what"]));
        assert_eq!(item["properties"]["what"]["type"], "string");
        assert_eq!(
            item["properties"]["type"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            item["properties"]["type"]["enum"],
            json!(["error", "plan", null])
        );
        assert_eq!(
            item["properties"]["links"]["type"],
            json!(["array", "null"])
        );
        assert_eq!(
            item["properties"]["links"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn null_answers_for_optional_fields_are_dropped() {
        let answer = json!({"facts":[{"what":"x","links":null,"status_evidence":null}]});
        assert_eq!(without_nulls(answer), json!({"facts":[{"what":"x"}]}));
    }
}
