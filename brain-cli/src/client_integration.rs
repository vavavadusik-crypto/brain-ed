//! R1: a Brain MCP entry in the `config.toml` of one explicitly registered
//! Codex home (`brain clients configure|unconfigure --label L`).
//!
//! Scope is deliberately narrow: only `config.toml`, only a marked
//! `[mcp_servers.brain]` block, no hooks, `AGENTS.md`, profiles, trust state,
//! auth or env. Configured is not a handshake, hook activity or capture.
//!
//! Crash safety: a private intent (hashes and fixed names only, never user
//! content) is made durable before the first change to the integration binary
//! or the target. Progress is derived only from the target's current hash
//! (`from`, `to` or neither), so any interrupted run is finished or rolled
//! back by the next run under the registry lock. There is no promise of
//! atomicity across files; a crash can leave an owner-only temporary file next
//! to `config.toml` (documented).

use std::error::Error;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client_registry::{self, CodexHome, Identity};

type Failure = Box<dyn Error>;

const MAX_STATE_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const CONFIG_FILE: &str = "config.toml";
const BEGIN: &str = "# BRAIN_REGISTERED_MCP_BEGIN";
const END: &str = "# BRAIN_REGISTERED_MCP_END";
const LEGACY_BEGIN: &str = "# BRAIN_FOR_AI_BEGIN";
const LEGACY_END: &str = "# BRAIN_FOR_AI_END";
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NONBLOCK: i32 = 0o4000;

fn fail(why: &str) -> Failure {
    why.to_string().into()
}

fn refuse(why: &str) -> Failure {
    format!("refusing: {why}; nothing was changed").into()
}

/// Fixed wording: the offending path is never quoted.
const UNSAFE_HOME: &str = "the home directory and the Brain home must be absolute UTF-8 paths without control characters (set HOME to an absolute path)";

/// Absolute, UTF-8 and free of control characters (DEL included). Nothing is
/// resolved or followed, so an existing alias such as `/home -> /var/home`
/// stays acceptable. A relative path would make the MCP command resolve
/// against the client's working directory instead of the copied binary, and a
/// control character cannot be written into TOML.
fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| !text.chars().any(char::is_control))
}

/// Runs before any lock, state read, target read, copy or write; dry runs
/// are refused the same way.
fn check_context(ctx: &Context) -> Result<(), Failure> {
    if safe_absolute(ctx.home) && safe_absolute(ctx.brain_home) {
        Ok(())
    } else {
        Err(refuse(UNSAFE_HOME))
    }
}

/// Where the operation runs.
pub(crate) struct Context<'a> {
    pub(crate) home: &'a Path,
    pub(crate) brain_home: &'a Path,
    pub(crate) source: Source,
}

/// The executable bytes the integration binary is copied from.
pub(crate) enum Source {
    /// The image this process is running, read through one handle on the
    /// kernel's `/proc/self/exe` (Linux only; elsewhere a refusal). A path name
    /// such as `current_exe()` is never trusted: it can name newer bytes.
    RunningImage,
    /// Tests: an owned synthetic ELF file.
    #[cfg(test)]
    Path(PathBuf),
}

fn open_source(source: &Source) -> io::Result<fs::File> {
    match source {
        #[cfg(target_os = "linux")]
        Source::RunningImage => fs::File::open("/proc/self/exe"),
        #[cfg(not(target_os = "linux"))]
        Source::RunningImage => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the running image cannot be read on this platform",
        )),
        #[cfg(test)]
        Source::Path(path) => fs::File::open(path),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Configure,
    Unconfigure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TargetFile {
    Config,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileState {
    exists: bool,
    sha256: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    version: u32,
    op: Op,
    label: String,
    client: String,
    identity: Identity,
    file: TargetFile,
    from: FileState,
    to: FileState,
    binary_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    label: String,
    client: String,
    identity: Identity,
    file: TargetFile,
    original: FileState,
    configured: FileState,
    binary_sha256: String,
}

/// Test-only failure injection after each durable step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Intent,
    Binary,
    Config,
    Manifest,
    UnIntent,
    UnConfig,
    UnManifest,
}

#[cfg(test)]
thread_local! {
    static FAIL_AT: std::cell::Cell<Option<Stage>> = const { std::cell::Cell::new(None) };
}

fn failpoint(stage: Stage) -> Result<(), Failure> {
    #[cfg(test)]
    if FAIL_AT.with(|cell| cell.get()) == Some(stage) {
        return Err(fail("injected failure"));
    }
    let _ = stage;
    Ok(())
}

struct Paths {
    dir: PathBuf,
    bin_dir: PathBuf,
    binary: PathBuf,
    manifest: PathBuf,
    intent: PathBuf,
}

fn paths(brain_home: &Path, label: &str) -> Paths {
    let dir = brain_home.join("integrations");
    Paths {
        bin_dir: dir.join("bin"),
        binary: dir.join("bin").join("brain"),
        manifest: dir.join(format!("{label}.json")),
        intent: dir.join(format!("{label}.intent.json")),
        dir,
    }
}

fn sha256(bytes: &[u8]) -> String {
    crate::installer::sha256(bytes)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_file_state(state: &FileState) -> bool {
    match &state.sha256 {
        Some(sha) => state.exists && is_sha256(sha),
        None => !state.exists,
    }
}

fn state_of(bytes: Option<&[u8]>) -> FileState {
    FileState {
        exists: bytes.is_some(),
        sha256: bytes.map(sha256),
    }
}

#[cfg(unix)]
fn owner_only(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    crate::private_fs::current_uid() == Some(metadata.uid()) && metadata.mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn owner_only(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_inode(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

/// A regular, owner-only file, read through one bounded no-follow,
/// non-blocking handle that must be the checked inode. `Ok(None)` only for
/// an exact `NotFound`.
fn read_owned(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, &'static str> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot be inspected"),
    };
    if !before.file_type().is_file() {
        return Err("not a regular file");
    }
    if !owner_only(&before) {
        return Err("not owner-only or owned by another user");
    }
    if before.len() > limit {
        return Err("too large");
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| "cannot be opened safely")?;
    let opened = file.metadata().map_err(|_| "cannot be inspected")?;
    if !opened.file_type().is_file() || !same_inode(&before, &opened) {
        return Err("changed while it was opened");
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot be read")?;
    if bytes.len() as u64 > limit {
        return Err("too large");
    }
    Ok(Some(bytes))
}

/// `Ok(false)` only for an exact `NotFound`; otherwise a real owner-only
/// directory.
fn private_dir(path: &Path) -> Result<bool, &'static str> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() && owner_only(&metadata) => Ok(true),
        Ok(_) => Err("integration state directory is not a private directory owned by you"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("integration state directory cannot be inspected"),
    }
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Owner-only temporary file in `dir` (created exclusively, never followed),
/// fsynced, then `check` runs, then it is renamed over `target` and `dir` is
/// fsynced. The temporary file is removed on every error this call sees.
fn write_atomic(
    dir: &Path,
    target: &Path,
    bytes: &[u8],
    mode: u32,
    check: &dyn Fn() -> Result<(), Failure>,
) -> Result<(), Failure> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = dir.join(format!(".{name}.brain-tmp-{}-{nonce}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
        options.custom_flags(O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options
        .open(&temporary)
        .map_err(|_| fail("a temporary file could not be created safely"))?;
    let result = (|| -> Result<(), Failure> {
        file.write_all(bytes)?;
        file.sync_all()?;
        check()?;
        fs::rename(&temporary, target)?;
        sync_dir(dir)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_durable(path: &Path, dir: &Path) -> Result<(), Failure> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_dir(dir)?;
    Ok(())
}

fn load_state<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Failure> {
    let bytes = read_owned(path, MAX_STATE_BYTES)
        .map_err(|why| refuse(&format!("an integration state file is {why}")))?;
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| refuse("an integration state file is not valid"))
}

fn save_state<T: Serialize>(dir: &Path, path: &Path, value: &T) -> Result<(), Failure> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(fail("integration state would exceed its size limit"));
    }
    write_atomic(dir, path, &bytes, 0o600, &|| Ok(()))
}

fn check_intent(intent: &Intent, label: &str, identity: Identity) -> Result<(), Failure> {
    let shape = intent.version == 1
        && intent.label == label
        && intent.client == "codex"
        && valid_file_state(&intent.from)
        && valid_file_state(&intent.to)
        && intent.from != intent.to
        && match intent.op {
            Op::Configure => intent.to.exists,
            Op::Unconfigure => intent.from.exists,
        }
        && is_sha256(&intent.binary_sha256);
    if !shape {
        return Err(refuse("the pending integration state is not valid"));
    }
    if intent.identity != identity {
        return Err(refuse(
            "the registered home changed since this operation started; the pending state is kept",
        ));
    }
    Ok(())
}

fn check_manifest(manifest: &Manifest, label: &str, identity: Identity) -> Result<(), Failure> {
    let shape = manifest.version == 1
        && manifest.label == label
        && manifest.client == "codex"
        && valid_file_state(&manifest.original)
        && valid_file_state(&manifest.configured)
        && manifest.configured.exists
        && manifest.original != manifest.configured
        && is_sha256(&manifest.binary_sha256);
    if !shape {
        return Err(refuse("the integration record is not valid"));
    }
    if manifest.identity != identity {
        return Err(refuse(
            "the registered home changed since it was configured; the record is kept",
        ));
    }
    Ok(())
}

/// The block Brain inserts; the command is TOML-escaped, no shell involved.
fn block(binary: &str) -> String {
    format!(
        "{BEGIN}\n[mcp_servers.brain]\ncommand = {}\nargs = [\"serve\"]\n{END}\n",
        crate::installer::toml_string(binary)
    )
}

fn insert(original: &str, block: &str) -> Result<String, &'static str> {
    if [BEGIN, END, LEGACY_BEGIN, LEGACY_END]
        .iter()
        .any(|marker| original.contains(marker))
    {
        return Err("config.toml already contains Brain markers; remove them by hand first");
    }
    // The shared checker also refuses `[[hooks.*]]` arrays because the
    // default integration adds a `hooks` table; R1 adds no hooks, so user hook
    // arrays (the official inline form) are renamed for the check only. The
    // text that is written is the untouched original.
    // Only the plain form `[[hooks]]` or `[[hooks.<bare-key>]]` on a line of
    // its own is renamed; quoted keys, comments after the header or any
    // other spelling stay as they are and are refused by the checker.
    let simple_hook_header = |line: &str| {
        let line = line.trim();
        line == "[[hooks]]"
            || line
                .strip_prefix("[[hooks.")
                .and_then(|rest| rest.strip_suffix("]]"))
                .is_some_and(|key| {
                    !key.is_empty()
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                })
    };
    let checked = original
        .split('\n')
        .map(|line| {
            if simple_hook_header(line) {
                line.replacen("[[hooks", "[[user_hooks", 1)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    crate::installer::codex_config_conflicts(&checked).map_err(|_| {
        "config.toml has a construct Brain cannot edit safely (ambiguous TOML or an existing brain MCP entry)"
    })?;
    let separator = if original.is_empty() || original.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let configured = format!("{original}{separator}{block}");
    // The result must stay readable by `unconfigure` (same 1 MiB bound).
    if configured.len() as u64 > MAX_CONFIG_BYTES {
        return Err(
            "config.toml with the Brain block would exceed 1 MiB (the client config has grown unusually large; clean it up before configuring)",
        );
    }
    Ok(configured)
}

/// Inverse of `insert`, accepted only when the result has the recorded
/// original hash. `Ok(None)` means the file did not exist before.
fn remove(configured: &str, block: &str, original: &FileState) -> Result<Option<String>, ()> {
    if configured.matches(block).count() != 1 || !configured.ends_with(block) {
        return Err(());
    }
    let base = &configured[..configured.len() - block.len()];
    match &original.sha256 {
        None if base.is_empty() => Ok(None),
        None => Err(()),
        Some(sha) => {
            if sha256(base.as_bytes()) == *sha {
                return Ok(Some(base.to_string()));
            }
            match base.strip_suffix('\n') {
                Some(trimmed) if sha256(trimmed.as_bytes()) == *sha => {
                    Ok(Some(trimmed.to_string()))
                }
                _ => Err(()),
            }
        }
    }
}

fn binary_path(paths: &Paths) -> Result<String, Failure> {
    if !safe_absolute(&paths.binary) {
        return Err(refuse(UNSAFE_HOME));
    }
    paths
        .binary
        .to_str()
        .map(str::to_string)
        .ok_or_else(|| refuse(UNSAFE_HOME))
}

/// The running executable: a regular x86-64 ELF executable within the size
/// bound, hashed from exactly the bytes that may later be copied.
fn read_source(source: &Source) -> Result<(Vec<u8>, String), Failure> {
    let invalid = || refuse("the running Brain executable cannot be copied safely");
    // Metadata and bytes come from the same open handle.
    let file = open_source(source).map_err(|_| invalid())?;
    let metadata = file.metadata().map_err(|_| invalid())?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_BINARY_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    let elf = bytes.len() >= 64
        && bytes.len() as u64 <= MAX_BINARY_BYTES
        && bytes.starts_with(b"\x7fELF")
        && bytes[4] == 2
        && bytes[5] == 1
        && matches!(u16::from_le_bytes([bytes[16], bytes[17]]), 2 | 3)
        && u16::from_le_bytes([bytes[18], bytes[19]]) == 0x3e;
    if !elf {
        return Err(invalid());
    }
    let sha = sha256(&bytes);
    Ok((bytes, sha))
}

/// The integration binary's hash, if it exists as a private owned
/// executable; anything else there is a refusal.
fn installed_binary(paths: &Paths, bin_dir_exists: bool) -> Result<Option<String>, Failure> {
    if !bin_dir_exists {
        return Ok(None);
    }
    #[cfg(unix)]
    if let Ok(metadata) = fs::symlink_metadata(&paths.binary) {
        use std::os::unix::fs::PermissionsExt;
        if metadata.file_type().is_file() && metadata.permissions().mode() & 0o100 == 0 {
            return Err(refuse(
                "the integration binary is not executable (check permissions on the installed brain binary and re-run `brain init --agents`)",
            ));
        }
    }
    let bytes = read_owned(&paths.binary, MAX_BINARY_BYTES)
        .map_err(|why| refuse(&format!("the integration binary is {why}")))?;
    Ok(bytes.map(|bytes| sha256(&bytes)))
}

struct Target {
    path: PathBuf,
    text: Option<String>,
    state: FileState,
}

fn read_target(registered: &CodexHome) -> Result<Target, Failure> {
    let path = registered.home.join(CONFIG_FILE);
    let bytes = read_owned(&path, MAX_CONFIG_BYTES)
        .map_err(|why| refuse(&format!("config.toml is {why}")))?;
    let state = state_of(bytes.as_deref());
    let text = match bytes {
        Some(bytes) => {
            Some(String::from_utf8(bytes).map_err(|_| refuse("config.toml is not UTF-8"))?)
        }
        None => None,
    };
    Ok(Target { path, text, state })
}

/// Right before a target rename or removal: the registration is still the
/// same directory and the target still has the state that was read.
fn still_same(ctx: &Context, registered: &CodexHome, expected: &FileState) -> Result<(), Failure> {
    let again = client_registry::registered_codex(ctx.home, ctx.brain_home, &registered.label)?;
    if again.identity != registered.identity || again.home != registered.home {
        return Err(refuse("the registered home changed during the operation"));
    }
    if read_target(&again)?.state != *expected {
        return Err(refuse("config.toml changed during the operation"));
    }
    Ok(())
}

fn write_target(
    ctx: &Context,
    registered: &CodexHome,
    target: &Target,
    content: Option<&str>,
) -> Result<(), Failure> {
    let check = || still_same(ctx, registered, &target.state);
    match content {
        Some(text) => write_atomic(
            &registered.home,
            &target.path,
            text.as_bytes(),
            0o600,
            &check,
        ),
        None => {
            check()?;
            remove_durable(&target.path, &registered.home)
        }
    }
}

struct Own {
    dir_exists: bool,
    bin_dir_exists: bool,
    intent: Option<Intent>,
    manifest: Option<Manifest>,
}

fn own_state(paths: &Paths, label: &str, identity: Identity) -> Result<Own, Failure> {
    let dir_exists = private_dir(&paths.dir).map_err(refuse)?;
    let bin_dir_exists = dir_exists && private_dir(&paths.bin_dir).map_err(refuse)?;
    let (intent, manifest) = if dir_exists {
        (
            load_state::<Intent>(&paths.intent)?,
            load_state::<Manifest>(&paths.manifest)?,
        )
    } else {
        (None, None)
    };
    if let Some(intent) = &intent {
        check_intent(intent, label, identity)?;
    }
    if let Some(manifest) = &manifest {
        check_manifest(manifest, label, identity)?;
    }
    // A record next to a pending state must be its exact counterpart.
    if let (Some(intent), Some(manifest)) = (&intent, &manifest) {
        let consistent = match intent.op {
            Op::Configure => *manifest == manifest_from(intent),
            Op::Unconfigure => {
                manifest.configured == intent.from
                    && manifest.original == intent.to
                    && manifest.binary_sha256 == intent.binary_sha256
            }
        };
        if !consistent {
            return Err(refuse(
                "the integration record disagrees with the pending state; both are kept",
            ));
        }
    }
    Ok(Own {
        dir_exists,
        bin_dir_exists,
        intent,
        manifest,
    })
}

fn manifest_from(intent: &Intent) -> Manifest {
    Manifest {
        version: 1,
        label: intent.label.clone(),
        client: "codex".into(),
        identity: intent.identity,
        file: TargetFile::Config,
        original: intent.from.clone(),
        configured: intent.to.clone(),
        binary_sha256: intent.binary_sha256.clone(),
    }
}

fn report(label: &str, status: &str, action: &str, block: &str, written: usize) -> Value {
    json!({
        "status": status,
        "label": label,
        "client": "codex",
        "file": CONFIG_FILE,
        "action": action,
        "mcp_block": block,
        "files_written": written,
        "note": "configured MCP entry only; not a handshake, hook activity or capture",
    })
}

pub(crate) fn configure(ctx: &Context, label: &str, dry_run: bool) -> Result<Value, Failure> {
    check_context(ctx)?;
    let _lock = if dry_run {
        None
    } else {
        Some(client_registry::acquire_lock(ctx.brain_home)?)
    };
    let registered = client_registry::registered_codex(ctx.home, ctx.brain_home, label)?;
    let paths = paths(ctx.brain_home, label);
    let own = own_state(&paths, label, registered.identity)?;
    if own.intent.as_ref().is_some_and(|i| i.op == Op::Unconfigure) {
        return Err(refuse(
            "an unconfigure is pending for this label; run `brain clients unconfigure --yes` to finish it",
        ));
    }
    let (bytes, source_sha) = read_source(&ctx.source)?;
    if let Some(installed) = installed_binary(&paths, own.bin_dir_exists)?
        && installed != source_sha
    {
        return Err(refuse(
            "a different integration binary is already installed (no automatic upgrade): run `brain uninstall` for that client, then configure again",
        ));
    }
    let target = read_target(&registered)?;
    let block = block(&binary_path(&paths)?);
    let current = target.text.as_deref().unwrap_or("");

    // Decide everything before any write.
    enum Plan {
        Fresh(Intent, String),
        Resume(String),
        Bookkeeping,
        Unchanged,
    }
    let plan = match (&own.intent, &own.manifest) {
        (Some(intent), _) if intent.op == Op::Unconfigure => {
            return Err(refuse(
                "an unconfigure is pending for this label; run `brain clients unconfigure --yes` to finish it",
            ));
        }
        (Some(intent), manifest) => {
            if intent.binary_sha256 != source_sha {
                return Err(refuse(
                    "the pending configure was started by a different Brain executable; the pending state is kept",
                ));
            }
            if manifest
                .as_ref()
                .is_some_and(|m| *m != manifest_from(intent))
            {
                return Err(refuse(
                    "the integration record disagrees with the pending state",
                ));
            }
            if target.state == intent.to {
                Plan::Bookkeeping
            } else if target.state == intent.from {
                let after = insert(current, &block).map_err(refuse)?;
                if state_of(Some(after.as_bytes())) != intent.to {
                    return Err(refuse(
                        "the pending change can no longer be reproduced; the pending state is kept",
                    ));
                }
                Plan::Resume(after)
            } else {
                return Err(refuse(
                    "config.toml changed while a configure was pending; the pending state is kept",
                ));
            }
        }
        (None, Some(manifest)) => {
            if manifest.binary_sha256 != source_sha {
                return Err(refuse(
                    "this label was configured by a different Brain executable (no automatic upgrade): `brain clients unconfigure <label>` first, then configure again",
                ));
            }
            if target.state != manifest.configured {
                return Err(refuse(
                    "config.toml was changed after it was configured; edit it by hand or unconfigure",
                ));
            }
            Plan::Unchanged
        }
        (None, None) => {
            let after = insert(current, &block).map_err(refuse)?;
            let intent = Intent {
                version: 1,
                op: Op::Configure,
                label: label.to_string(),
                client: "codex".into(),
                identity: registered.identity,
                file: TargetFile::Config,
                from: target.state.clone(),
                to: state_of(Some(after.as_bytes())),
                binary_sha256: source_sha.clone(),
            };
            Plan::Fresh(intent, after)
        }
    };
    let action = match &plan {
        Plan::Fresh(..) if target.state.exists => "change",
        Plan::Fresh(..) => "create",
        Plan::Resume(_) => "resume_pending",
        Plan::Bookkeeping => "finish_pending",
        Plan::Unchanged => "unchanged",
    };
    if dry_run {
        return Ok(report(label, "dry_run", action, &block, 0));
    }

    let mut written = 0;
    if !own.dir_exists {
        create_private_dir(&paths.dir)?;
    }
    let intent = match plan {
        Plan::Fresh(intent, after) => {
            save_state(&paths.dir, &paths.intent, &intent)?;
            written += 1;
            failpoint(Stage::Intent)?;
            Some((intent, Some(after)))
        }
        Plan::Resume(after) => Some((own.intent.clone().expect("pending intent"), Some(after))),
        Plan::Bookkeeping => Some((own.intent.clone().expect("pending intent"), None)),
        Plan::Unchanged => None,
    };
    // The binary is created only after the intent is durable, and only from
    // the exact bytes hashed above.
    if !own.bin_dir_exists {
        create_private_dir(&paths.bin_dir)?;
    }
    if installed_binary(&paths, true)?.is_none() {
        write_atomic(&paths.bin_dir, &paths.binary, &bytes, 0o700, &|| Ok(()))?;
        written += 1;
    }
    failpoint(Stage::Binary)?;
    let Some((intent, after)) = intent else {
        return Ok(report(label, "configured", action, &block, written));
    };
    if let Some(after) = after {
        write_target(ctx, &registered, &target, Some(&after))?;
        written += 1;
    }
    failpoint(Stage::Config)?;
    if own.manifest.is_none() {
        save_state(&paths.dir, &paths.manifest, &manifest_from(&intent))?;
        written += 1;
    }
    failpoint(Stage::Manifest)?;
    remove_durable(&paths.intent, &paths.dir)?;
    Ok(report(label, "configured", action, &block, written))
}

pub(crate) fn unconfigure(ctx: &Context, label: &str, dry_run: bool) -> Result<Value, Failure> {
    check_context(ctx)?;
    let _lock = if dry_run {
        None
    } else {
        Some(client_registry::acquire_lock(ctx.brain_home)?)
    };
    let registered = client_registry::registered_codex(ctx.home, ctx.brain_home, label)?;
    let paths = paths(ctx.brain_home, label);
    let own = own_state(&paths, label, registered.identity)?;
    let target = read_target(&registered)?;
    let block = block(&binary_path(&paths)?);
    let current = target.text.as_deref().unwrap_or("");
    let restore = |original: &FileState| -> Result<Option<String>, Failure> {
        remove(current, &block, original)
            .map_err(|()| refuse("the original config.toml cannot be restored exactly"))
    };

    enum Plan {
        /// Write the unconfigure intent, then restore, then drop the record.
        Fresh(Intent, Option<String>),
        /// Finish an interrupted unconfigure.
        Resume(Option<Option<String>>),
        /// An interrupted configure that never changed config.toml.
        DropConfigure,
        /// An interrupted configure that did: record it first (never
        /// rewriting the pending configure in place), then unconfigure.
        CompleteThenUndo(Manifest, Option<String>),
        NotConfigured,
    }
    let unconfigure_intent = |manifest: &Manifest| Intent {
        version: 1,
        op: Op::Unconfigure,
        label: label.to_string(),
        client: "codex".into(),
        identity: registered.identity,
        file: TargetFile::Config,
        from: manifest.configured.clone(),
        to: manifest.original.clone(),
        binary_sha256: manifest.binary_sha256.clone(),
    };
    let plan = match (&own.intent, &own.manifest) {
        (Some(intent), _) if intent.op == Op::Unconfigure => {
            if target.state == intent.to {
                Plan::Resume(None)
            } else if target.state == intent.from {
                Plan::Resume(Some(restore(&intent.to)?))
            } else {
                return Err(refuse(
                    "config.toml changed while an unconfigure was pending; the pending state is kept",
                ));
            }
        }
        (Some(intent), manifest) => {
            if manifest
                .as_ref()
                .is_some_and(|m| *m != manifest_from(intent))
            {
                return Err(refuse(
                    "the integration record disagrees with the pending state",
                ));
            }
            if target.state == intent.from && manifest.is_none() {
                Plan::DropConfigure
            } else if target.state == intent.to {
                Plan::CompleteThenUndo(manifest_from(intent), restore(&intent.from)?)
            } else {
                return Err(refuse(
                    "config.toml changed while a configure was pending; the pending state is kept",
                ));
            }
        }
        (None, Some(manifest)) => {
            if target.state != manifest.configured {
                return Err(refuse(
                    "config.toml was changed after it was configured; it is left as it is",
                ));
            }
            Plan::Fresh(unconfigure_intent(manifest), restore(&manifest.original)?)
        }
        (None, None) => Plan::NotConfigured,
    };
    let action = match &plan {
        Plan::Fresh(_, Some(_)) | Plan::CompleteThenUndo(_, Some(_)) => "restore",
        Plan::Fresh(_, None) | Plan::CompleteThenUndo(_, None) => "remove_created_file",
        Plan::Resume(Some(_)) => "resume_pending",
        Plan::Resume(None) => "finish_pending",
        Plan::DropConfigure => "drop_pending_configure",
        Plan::NotConfigured => "not_configured",
    };
    if dry_run || matches!(plan, Plan::NotConfigured) {
        let status = if dry_run { "dry_run" } else { "not_configured" };
        return Ok(report(label, status, action, &block, 0));
    }

    let mut written = 0;
    let restored = match plan {
        Plan::DropConfigure => {
            remove_durable(&paths.intent, &paths.dir)?;
            return Ok(report(label, "unconfigured", action, &block, 1));
        }
        Plan::CompleteThenUndo(manifest, restored) => {
            save_state(&paths.dir, &paths.manifest, &manifest)?;
            remove_durable(&paths.intent, &paths.dir)?;
            save_state(&paths.dir, &paths.intent, &unconfigure_intent(&manifest))?;
            written += 3;
            Some(restored)
        }
        Plan::Fresh(intent, restored) => {
            save_state(&paths.dir, &paths.intent, &intent)?;
            written += 1;
            Some(restored)
        }
        Plan::Resume(restored) => restored,
        Plan::NotConfigured => unreachable!(),
    };
    failpoint(Stage::UnIntent)?;
    if let Some(restored) = restored {
        write_target(ctx, &registered, &target, restored.as_deref())?;
        written += 1;
    }
    failpoint(Stage::UnConfig)?;
    remove_durable(&paths.manifest, &paths.dir)?;
    failpoint(Stage::UnManifest)?;
    remove_durable(&paths.intent, &paths.dir)?;
    // The integration binary is intentionally retained (R1 contract).
    Ok(report(label, "unconfigured", action, &block, written))
}

/// For `forget`: any record or pending state for this label, valid or not,
/// blocks it. Only fixed names for this label are inspected.
pub(crate) fn forget_blocker(brain_home: &Path, label: &str) -> Option<&'static str> {
    let paths = paths(brain_home, label);
    match fs::symlink_metadata(&paths.dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return Some("the integration state cannot be inspected"),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Some("the integration state directory is not a real directory");
        }
        Ok(_) => {}
    }
    for path in [&paths.manifest, &paths.intent] {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Some("the integration state cannot be inspected"),
            Ok(_) => {
                return Some(
                    "a Codex MCP integration (or a pending one) exists for this label; run `brain clients unconfigure --yes` first",
                );
            }
        }
    }
    None
}

/// Doctor: Brain's own record and pending state, read bounded and validated
/// against the registration identity. No client file is opened, so
/// `configured` means "a valid record exists", not "Codex uses it".
pub(crate) fn status(brain_home: &Path, label: &str, identity: Identity) -> &'static str {
    // A relative or control-character Brain home is never read from.
    if !safe_absolute(brain_home) {
        return "unsafe_state";
    }
    match own_state(&paths(brain_home, label), label, identity) {
        Err(_) => "unsafe_state",
        Ok(own) if own.intent.is_some() => "pending_recovery",
        Ok(own) if own.manifest.is_some() => "configured",
        Ok(_) => "none",
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::client_registry::{ClientKind, ClientsCommand};
    use std::os::unix::fs::PermissionsExt;

    const ORIGINAL: &str = "# my settings\nmodel = \"x\"\n\n[mcp_servers.other]\ncommand = \"other\"\nenv = { TOKEN = \"SYNTH-SECRET-TOKEN\" }\n\n[[hooks.PreToolUse]]\nmatcher = \"x\"\n";

    struct Fixture {
        root: PathBuf,
        home: PathBuf,
        brain_home: PathBuf,
        codex: PathBuf,
        source: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn private(path: &Path) {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn elf(seed: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; 128];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[16] = 3;
        bytes[18] = 0x3e;
        bytes[100] = seed;
        bytes
    }

    fn fixture(label: &str) -> Fixture {
        fixture_named(label, "home")
    }

    /// The synthetic HOME is `<private temp root>/<home_name>`.
    fn fixture_named(_label: &str, home_name: &str) -> Fixture {
        let _nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = crate::test_support::temp_path("brain-r1");
        let home = root.join(home_name);
        let codex = home.join("work-codex");
        fs::create_dir_all(&codex).unwrap();
        for dir in [&root, &home, &codex] {
            private(dir);
        }
        let source = root.join("brain-source");
        fs::write(&source, elf(1)).unwrap();
        let brain_home = home.join(".brain");
        let fixture = Fixture {
            root,
            home,
            brain_home,
            codex,
            source,
        };
        client_registry::run(
            &ClientsCommand::Register {
                client: ClientKind::Codex,
                home: fixture.codex.clone(),
                label: "work".into(),
                dry_run: false,
            },
            &fixture.home,
            &fixture.brain_home,
            1,
        )
        .unwrap();
        fixture
    }

    fn ctx(f: &Fixture) -> Context<'_> {
        Context {
            home: &f.home,
            brain_home: &f.brain_home,
            source: Source::Path(f.source.clone()),
        }
    }

    fn ident(f: &Fixture) -> Identity {
        client_registry::registered_codex(&f.home, &f.brain_home, "work")
            .unwrap()
            .identity
    }

    fn edit_json(path: &Path, edit: impl Fn(&mut Value)) {
        let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        edit(&mut value);
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn configured_output_is_bounded_before_any_write() {
        let limit = MAX_CONFIG_BYTES as usize;
        // '#'*n without a newline: the result is n + 1 + block bytes.
        for (label, offset, accepted) in [
            ("under", 2_usize, true),
            ("at", 1, true),
            ("over", 0, false),
            ("root case", usize::MAX, false),
        ] {
            let f = fixture("bound");
            let block_len = block(&binary_path(&paths(&f.brain_home, "work")).unwrap()).len();
            let n = if offset == usize::MAX {
                limit
            } else {
                limit - block_len - offset
            };
            let original = "#".repeat(n);
            write_config(&f, &original);
            let before = tree(&f.root);
            let result = configure(&ctx(&f), "work", false);
            if accepted {
                result.unwrap();
                assert!(config(&f).unwrap().len() <= limit, "{label}");
                unconfigure(&ctx(&f), "work", false).unwrap();
                assert_eq!(config(&f).unwrap(), original.as_bytes(), "{label}");
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("exceed 1 MiB"), "{label}: {error}");
                assert_eq!(
                    tree(&f.root),
                    before,
                    "{label}: no intent, binary or target write"
                );
            }
        }
    }

    #[test]
    fn default_codex_symlink_alias_and_broken_default_are_refused() {
        let f = fixture("alias");
        write_config(&f, ORIGINAL);
        let default = f.home.join(".codex");
        std::os::unix::fs::symlink(&f.codex, &default).unwrap();
        let before = tree(&f.root);
        let error = configure(&ctx(&f), "work", false).unwrap_err().to_string();
        assert!(error.contains("default"), "{error}");
        assert_eq!(tree(&f.root), before);
        fs::remove_file(&default).unwrap();
        std::os::unix::fs::symlink(f.root.join("nowhere"), &default).unwrap();
        let error = configure(&ctx(&f), "work", false).unwrap_err().to_string();
        assert!(error.contains("cannot be resolved"), "{error}");
        assert!(!f.brain_home.join("integrations").exists());
        fs::remove_file(&default).unwrap();
        fs::create_dir(&default).unwrap();
        private(&default);
        configure(&ctx(&f), "work", false).unwrap();
        assert!(
            String::from_utf8(config(&f).unwrap())
                .unwrap()
                .contains(BEGIN)
        );
    }

    #[test]
    fn semantically_invalid_or_mismatched_state_is_refused_and_kept() {
        type Edit = Box<dyn Fn(&mut Value)>;
        let edits: Vec<(&str, Edit)> = vec![
            (
                "configure to absent",
                Box::new(|v| {
                    v["to"] = serde_json::json!({"exists": false, "sha256": null});
                }),
            ),
            (
                "configure from == to",
                Box::new(|v| v["from"] = v["to"].clone()),
            ),
        ];
        for (label, edit) in edits {
            let f = fixture("semantic");
            write_config(&f, ORIGINAL);
            fail_at(Some(Stage::Binary));
            assert!(configure(&ctx(&f), "work", false).is_err());
            fail_at(None);
            let p = paths(&f.brain_home, "work");
            edit_json(&p.intent, edit);
            let before = tree(&f.root);
            assert!(configure(&ctx(&f), "work", false).is_err(), "{label}");
            assert!(unconfigure(&ctx(&f), "work", false).is_err(), "{label}");
            assert_eq!(
                status(&f.brain_home, "work", ident(&f)),
                "unsafe_state",
                "{label}"
            );
            assert_eq!(tree(&f.root), before, "{label}: target and state unchanged");
        }
        let unconfigure_edits: Vec<(&str, PathBuf, Edit)> = vec![
            (
                "record binary mismatch",
                PathBuf::from("manifest"),
                Box::new(|v| {
                    v["binary_sha256"] = "f".repeat(64).into();
                }),
            ),
            (
                "record original mismatch",
                PathBuf::from("manifest"),
                Box::new(|v| {
                    v["original"] = serde_json::json!({"exists": true, "sha256": "e".repeat(64)});
                }),
            ),
            (
                "unconfigure from absent",
                PathBuf::from("intent"),
                Box::new(|v| {
                    v["from"] = serde_json::json!({"exists": false, "sha256": null});
                }),
            ),
        ];
        for (label, which, edit) in unconfigure_edits {
            let f = fixture("semantic-un");
            write_config(&f, ORIGINAL);
            configure(&ctx(&f), "work", false).unwrap();
            fail_at(Some(Stage::UnIntent));
            assert!(unconfigure(&ctx(&f), "work", false).is_err());
            fail_at(None);
            let p = paths(&f.brain_home, "work");
            let target = if which == Path::new("manifest") {
                &p.manifest
            } else {
                &p.intent
            };
            edit_json(target, edit);
            let before = tree(&f.root);
            assert!(unconfigure(&ctx(&f), "work", false).is_err(), "{label}");
            assert!(configure(&ctx(&f), "work", false).is_err(), "{label}");
            assert_eq!(
                tree(&f.root),
                before,
                "{label}: bookkeeping is never dropped"
            );
        }
    }

    fn write_config(f: &Fixture, text: &str) {
        let path = f.codex.join(CONFIG_FILE);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn config(f: &Fixture) -> Option<Vec<u8>> {
        fs::read(f.codex.join(CONFIG_FILE)).ok()
    }

    /// Every file under the fixture root with its bytes and mode.
    fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, u32)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let mode = metadata.permissions().mode();
                if metadata.is_dir() {
                    stack.push(path.clone());
                    out.push((path, None, mode));
                } else if metadata.is_file() {
                    out.push((path.clone(), fs::read(&path).ok(), mode));
                } else {
                    // Never open a FIFO or other special file in a snapshot.
                    out.push((path, None, mode));
                }
            }
        }
        out.sort();
        out
    }

    fn fail_at(stage: Option<Stage>) {
        FAIL_AT.with(|cell| cell.set(stage));
    }

    fn grep_tree(root: &Path, needle: &[u8]) -> bool {
        tree(root).iter().any(|(path, bytes, _)| {
            !path.ends_with(CONFIG_FILE)
                && bytes
                    .as_ref()
                    .is_some_and(|b| b.windows(needle.len()).any(|w| w == needle))
        })
    }

    #[test]
    fn dry_runs_write_nothing_anywhere() {
        let f = fixture("dry");
        write_config(&f, ORIGINAL);
        let before = tree(&f.root);
        let plan = configure(&ctx(&f), "work", true).unwrap();
        assert_eq!(plan["status"], "dry_run");
        assert_eq!(plan["action"], "change");
        assert!(
            plan["mcp_block"]
                .as_str()
                .unwrap()
                .contains("[mcp_servers.brain]")
        );
        assert_eq!(
            unconfigure(&ctx(&f), "work", true).unwrap()["action"],
            "not_configured"
        );
        assert_eq!(
            tree(&f.root),
            before,
            "no directory, lock, intent, binary or target write"
        );
    }

    #[test]
    fn configure_and_unconfigure_restore_exact_bytes_without_leaking() {
        let f = fixture("roundtrip");
        write_config(&f, ORIGINAL);
        let result = configure(&ctx(&f), "work", false).unwrap();
        let text = String::from_utf8(config(&f).unwrap()).unwrap();
        assert!(text.starts_with(ORIGINAL), "user bytes preserved exactly");
        assert!(text.ends_with(&block(&binary_path(&paths(&f.brain_home, "work")).unwrap())));
        assert!(!text.contains("hooks.brain") && !text.contains("trusted_hash"));
        let output = result.to_string();
        assert!(!output.contains("SYNTH") && !output.contains(f.codex.to_str().unwrap()));
        assert!(
            !grep_tree(&f.brain_home, b"SYNTH"),
            "no copy of user content in .brain"
        );
        let p = paths(&f.brain_home, "work");
        assert!(p.manifest.exists() && !p.intent.exists());
        assert_eq!(
            fs::symlink_metadata(&p.binary)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(fs::read(&p.binary).unwrap(), elf(1));
        assert_eq!(
            configure(&ctx(&f), "work", false).unwrap()["action"],
            "unchanged"
        );
        let undone = unconfigure(&ctx(&f), "work", false).unwrap();
        assert_eq!(undone["status"], "unconfigured");
        assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes());
        assert!(!p.manifest.exists() && !p.intent.exists());
        assert!(p.binary.exists(), "integration binary is retained");
    }

    #[test]
    fn absent_and_unterminated_configs_round_trip() {
        for original in [None, Some("model = \"x\""), Some("")] {
            let f = fixture("absent");
            if let Some(text) = original {
                write_config(&f, text);
            }
            configure(&ctx(&f), "work", false).unwrap();
            assert!(config(&f).is_some());
            unconfigure(&ctx(&f), "work", false).unwrap();
            assert_eq!(
                config(&f),
                original.map(|t| t.as_bytes().to_vec()),
                "{original:?}"
            );
        }
    }

    #[test]
    fn every_configure_crash_point_recovers_forward_or_back() {
        for stage in [Stage::Intent, Stage::Binary, Stage::Config, Stage::Manifest] {
            for forward in [true, false] {
                let f = fixture("crash");
                write_config(&f, ORIGINAL);
                fail_at(Some(stage));
                assert!(configure(&ctx(&f), "work", false).is_err(), "{stage:?}");
                fail_at(None);
                let p = paths(&f.brain_home, "work");
                assert!(p.intent.exists(), "{stage:?}: the intent is kept");
                if forward {
                    configure(&ctx(&f), "work", false).unwrap();
                    assert!(p.manifest.exists() && !p.intent.exists(), "{stage:?}");
                    unconfigure(&ctx(&f), "work", false).unwrap();
                } else {
                    unconfigure(&ctx(&f), "work", false).unwrap();
                }
                assert_eq!(
                    config(&f).unwrap(),
                    ORIGINAL.as_bytes(),
                    "{stage:?} {forward}"
                );
                assert!(
                    !p.manifest.exists() && !p.intent.exists(),
                    "{stage:?} {forward}"
                );
            }
        }
    }

    #[test]
    fn every_unconfigure_crash_point_recovers_and_blocks_configure() {
        for stage in [Stage::UnIntent, Stage::UnConfig, Stage::UnManifest] {
            let f = fixture("uncrash");
            write_config(&f, ORIGINAL);
            configure(&ctx(&f), "work", false).unwrap();
            fail_at(Some(stage));
            assert!(unconfigure(&ctx(&f), "work", false).is_err(), "{stage:?}");
            fail_at(None);
            let error = configure(&ctx(&f), "work", false).unwrap_err().to_string();
            assert!(
                error.contains("unconfigure is pending"),
                "{stage:?}: {error}"
            );
            unconfigure(&ctx(&f), "work", false).unwrap();
            assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes(), "{stage:?}");
            let p = paths(&f.brain_home, "work");
            assert!(!p.manifest.exists() && !p.intent.exists(), "{stage:?}");
        }
    }

    #[test]
    fn user_drift_is_never_overwritten_and_state_is_kept() {
        let f = fixture("drift");
        write_config(&f, ORIGINAL);
        configure(&ctx(&f), "work", false).unwrap();
        let mut edited = config(&f).unwrap();
        edited.extend_from_slice(b"# my later edit\n");
        write_config(&f, std::str::from_utf8(&edited).unwrap());
        let p = paths(&f.brain_home, "work");
        let manifest = fs::read(&p.manifest).unwrap();
        assert!(unconfigure(&ctx(&f), "work", false).is_err());
        assert_eq!(config(&f).unwrap(), edited);
        assert_eq!(fs::read(&p.manifest).unwrap(), manifest);
        // Drift while a configure is pending keeps the intent.
        let g = fixture("drift-pending");
        write_config(&g, ORIGINAL);
        fail_at(Some(Stage::Binary));
        assert!(configure(&ctx(&g), "work", false).is_err());
        fail_at(None);
        write_config(&g, "changed = 1\n");
        let intent = fs::read(paths(&g.brain_home, "work").intent).unwrap();
        assert!(configure(&ctx(&g), "work", false).is_err());
        assert!(unconfigure(&ctx(&g), "work", false).is_err());
        assert_eq!(
            fs::read(paths(&g.brain_home, "work").intent).unwrap(),
            intent
        );
        assert_eq!(config(&g).unwrap(), b"changed = 1\n");
    }

    #[test]
    fn unsafe_or_ambiguous_targets_are_refused_before_any_write() {
        type Setup = Box<dyn Fn(&Fixture)>;
        let cases: Vec<(&str, Setup)> = vec![
            (
                "quoted hook array",
                Box::new(|f| write_config(f, "[[hooks.\"PreToolUse\"]]\nmatcher = \"x\"\n")),
            ),
            (
                "permissive",
                Box::new(|f| {
                    write_config(f, ORIGINAL);
                    fs::set_permissions(
                        f.codex.join(CONFIG_FILE),
                        fs::Permissions::from_mode(0o644),
                    )
                    .unwrap();
                }),
            ),
            (
                "symlink",
                Box::new(|f| {
                    let real = f.root.join("real.toml");
                    fs::write(&real, ORIGINAL).unwrap();
                    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
                    std::os::unix::fs::symlink(&real, f.codex.join(CONFIG_FILE)).unwrap();
                }),
            ),
            (
                "fifo",
                Box::new(|f| {
                    let status = std::process::Command::new("mkfifo")
                        .arg(f.codex.join(CONFIG_FILE))
                        .status()
                        .unwrap();
                    assert!(status.success());
                }),
            ),
            (
                "directory",
                Box::new(|f| fs::create_dir(f.codex.join(CONFIG_FILE)).unwrap()),
            ),
            (
                "oversize",
                Box::new(|f| write_config(f, &"#".repeat(MAX_CONFIG_BYTES as usize + 1))),
            ),
            (
                "not utf8",
                Box::new(|f| {
                    fs::write(f.codex.join(CONFIG_FILE), b"\xff\xfe").unwrap();
                    fs::set_permissions(
                        f.codex.join(CONFIG_FILE),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                }),
            ),
            (
                "own markers",
                Box::new(|f| write_config(f, &format!("{BEGIN}\n{END}\n"))),
            ),
            (
                "legacy markers",
                Box::new(|f| write_config(f, &format!("{LEGACY_BEGIN}\n{LEGACY_END}\n"))),
            ),
            (
                "existing brain mcp",
                Box::new(|f| write_config(f, "[mcp_servers.brain]\ncommand = \"x\"\n")),
            ),
            (
                "triple quoted",
                Box::new(|f| write_config(f, "x = \"\"\"\n[mcp_servers.brain]\n\"\"\"\n")),
            ),
        ];
        for (label, setup) in cases {
            let f = fixture("unsafe");
            setup(&f);
            let before = tree(&f.root);
            let error = configure(&ctx(&f), "work", false).unwrap_err().to_string();
            assert!(error.contains("refusing"), "{label}: {error}");
            assert!(
                !error.contains(f.codex.to_str().unwrap()),
                "{label}: no path"
            );
            assert_eq!(tree(&f.root), before, "{label}: nothing written");
        }
    }

    #[test]
    fn default_codex_home_and_changed_identity_are_refused() {
        let f = fixture("default");
        let default = f.home.join(".codex");
        fs::create_dir(&default).unwrap();
        private(&default);
        client_registry::run(
            &ClientsCommand::Register {
                client: ClientKind::Codex,
                home: default.clone(),
                label: "dflt".into(),
                dry_run: false,
            },
            &f.home,
            &f.brain_home,
            1,
        )
        .unwrap();
        let error = configure(&ctx(&f), "dflt", false).unwrap_err().to_string();
        assert!(error.contains("default"), "{error}");
        assert!(!f.brain_home.join("integrations").exists());
        // Replace the registered directory: identity no longer matches.
        fs::rename(&f.codex, f.root.join("moved")).unwrap();
        fs::create_dir(&f.codex).unwrap();
        private(&f.codex);
        assert!(configure(&ctx(&f), "work", false).is_err());
        assert!(!f.brain_home.join("integrations").exists());
    }

    #[test]
    fn unsafe_state_files_and_binaries_are_refused_before_target_reads() {
        let f = fixture("state");
        write_config(&f, ORIGINAL);
        configure(&ctx(&f), "work", false).unwrap();
        let p = paths(&f.brain_home, "work");
        let configured = config(&f).unwrap();
        let good = fs::read(&p.manifest).unwrap();
        let set = |bytes: &[u8], mode: u32| {
            let _ = fs::remove_file(&p.manifest);
            fs::write(&p.manifest, bytes).unwrap();
            fs::set_permissions(&p.manifest, fs::Permissions::from_mode(mode)).unwrap();
        };
        for (label, bytes, mode) in [
            ("garbage", b"{".to_vec(), 0o600),
            (
                "unknown field",
                String::from_utf8(good.clone())
                    .unwrap()
                    .replacen('{', "{\"x\":1,", 1)
                    .into_bytes(),
                0o600,
            ),
            ("permissive", good.clone(), 0o644),
            ("oversize", vec![b' '; MAX_STATE_BYTES as usize + 1], 0o600),
        ] {
            set(&bytes, mode);
            assert!(unconfigure(&ctx(&f), "work", false).is_err(), "{label}");
            assert!(configure(&ctx(&f), "work", false).is_err(), "{label}");
            assert_eq!(config(&f).unwrap(), configured, "{label}");
        }
        let _ = fs::remove_file(&p.manifest);
        let elsewhere = f.root.join("manifest-copy");
        fs::write(&elsewhere, &good).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &p.manifest).unwrap();
        assert!(unconfigure(&ctx(&f), "work", false).is_err(), "symlink");
        fs::remove_file(&p.manifest).unwrap();
        set(&good, 0o600);
        // A different source executable never replaces the installed copy.
        fs::write(&f.source, elf(2)).unwrap();
        let error = configure(&ctx(&f), "work", false).unwrap_err().to_string();
        assert!(error.contains("different"), "{error}");
        assert_eq!(fs::read(&p.binary).unwrap(), elf(1));
        // A missing copy is rehydrated only from the recorded executable.
        fs::remove_file(&p.binary).unwrap();
        assert!(configure(&ctx(&f), "work", false).is_err());
        assert!(!p.binary.exists());
        fs::write(&f.source, elf(1)).unwrap();
        configure(&ctx(&f), "work", false).unwrap();
        assert_eq!(fs::read(&p.binary).unwrap(), elf(1));
        // A non-ELF source is refused.
        fs::write(&f.source, b"#!/bin/sh\n").unwrap();
        assert!(read_source(&Source::Path(f.source.clone())).is_err());
    }

    #[test]
    fn forget_refuses_while_a_record_or_pending_state_exists() {
        let f = fixture("forget");
        write_config(&f, ORIGINAL);
        fail_at(Some(Stage::Intent));
        assert!(configure(&ctx(&f), "work", false).is_err());
        fail_at(None);
        for dry_run in [true, false] {
            let error = client_registry::run(
                &ClientsCommand::Forget {
                    label: "work".into(),
                    dry_run,
                },
                &f.home,
                &f.brain_home,
                1,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("unconfigure"), "{error}");
        }
        assert_eq!(status(&f.brain_home, "work", ident(&f)), "pending_recovery");
        configure(&ctx(&f), "work", false).unwrap();
        assert_eq!(status(&f.brain_home, "work", ident(&f)), "configured");
        let p = paths(&f.brain_home, "work");
        fs::write(&p.manifest, b"garbage").unwrap();
        assert!(
            forget_blocker(&f.brain_home, "work").is_some(),
            "corrupt still blocks"
        );
        assert!(unconfigure(&ctx(&f), "work", false).is_err());
    }

    #[test]
    fn concurrent_configure_and_forget_never_orphan() {
        let f = fixture("race");
        write_config(&f, ORIGINAL);
        let results = std::thread::scope(|scope| {
            let handles = (0..32)
                .map(|i| {
                    let f = &f;
                    scope.spawn(move || {
                        if i % 2 == 0 {
                            configure(&ctx(f), "work", false)
                                .map(|_| "configure")
                                .map_err(|e| e.to_string())
                        } else {
                            client_registry::run(
                                &ClientsCommand::Forget {
                                    label: "work".into(),
                                    dry_run: false,
                                },
                                &f.home,
                                &f.brain_home,
                                1,
                            )
                            .map(|_| "forget")
                            .map_err(|e| e.to_string())
                        }
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        let registered = client_registry::registered_codex(&f.home, &f.brain_home, "work").is_ok();
        let p = paths(&f.brain_home, "work");
        if !registered {
            assert!(
                !p.manifest.exists() && !p.intent.exists(),
                "no orphaned integration"
            );
            assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes());
        } else {
            assert!(!p.intent.exists());
        }
        for result in &results {
            if let Err(error) = result {
                assert!(
                    error.contains("in progress")
                        || error.contains("unconfigure")
                        || error.contains("no registration")
                        || error.contains("no registry"),
                    "{error}"
                );
            }
        }
    }

    /// Unset HOME reaches here as ".", empty HOME as "": both are modelled by
    /// injected relative contexts, so no environment or cwd is touched and the
    /// pure guard is exercised without any filesystem access.
    #[test]
    fn relative_empty_non_utf8_or_control_homes_fail_the_pure_guard() {
        use std::os::unix::ffi::OsStrExt;
        let non_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/abs/\xff"));
        let non_utf8_brain = non_utf8.join(".brain");
        let cases: Vec<(&Path, &Path)> = vec![
            (Path::new("."), Path::new("./.brain")),
            (Path::new(""), Path::new(".brain")),
            (Path::new("home"), Path::new("home/.brain")),
            (Path::new("/abs/home"), Path::new("relative/.brain")),
            (Path::new("relative"), Path::new("/abs/.brain")),
            (
                Path::new("/abs/h\u{7f}me"),
                Path::new("/abs/h\u{7f}me/.brain"),
            ),
            (Path::new("/abs/home"), Path::new("/abs/home\n/.brain")),
            (Path::new("/abs/\u{1b}[31m"), Path::new("/abs/x/.brain")),
            (non_utf8.as_path(), non_utf8_brain.as_path()),
        ];
        for (home, brain_home) in cases {
            let ctx = Context {
                home,
                brain_home,
                source: Source::Path(PathBuf::from("/nonexistent-brain-source")),
            };
            let error = check_context(&ctx).unwrap_err().to_string();
            assert!(
                error.contains("absolute UTF-8") && error.contains("nothing was changed"),
                "{home:?}: {error}"
            );
            for leaked in ["h\u{7f}me", "relative", "\u{1b}", "\n"] {
                assert!(!error.contains(leaked), "{home:?}: {error}");
            }
        }
        for brain_home in [".brain", "./.brain", "home/.brain", "/abs/h\u{7f}me/.brain"] {
            assert!(
                binary_path(&paths(Path::new(brain_home), "work")).is_err(),
                "{brain_home:?}"
            );
        }
        let unicode = Path::new("/abs/дом с пробелом");
        let ctx = Context {
            home: unicode,
            brain_home: &unicode.join(".brain"),
            source: Source::Path(PathBuf::from("/nonexistent-brain-source")),
        };
        assert!(check_context(&ctx).is_ok());
        assert!(binary_path(&paths(&unicode.join(".brain"), "work")).is_ok());
    }

    #[test]
    fn control_character_homes_refuse_every_operation_before_any_write() {
        let f = fixture("control");
        write_config(&f, ORIGINAL);
        let del_home = f.root.join("h\u{7f}me");
        let cases = [
            (del_home.clone(), del_home.join(".brain")),
            (f.home.clone(), f.home.join(".brain\u{1b}")),
            (del_home.clone(), f.brain_home.clone()),
            (f.home.clone(), f.root.join("b\u{7f}rain")),
        ];
        for configured in [false, true] {
            if configured {
                configure(&ctx(&f), "work", false).unwrap();
            }
            let before = tree(&f.root);
            for (home, brain_home) in &cases {
                for dry_run in [true, false] {
                    let c = Context {
                        home,
                        brain_home,
                        source: Source::Path(f.source.clone()),
                    };
                    for result in [
                        configure(&c, "work", dry_run),
                        unconfigure(&c, "work", dry_run),
                    ] {
                        let error = result.unwrap_err().to_string();
                        assert!(error.contains("absolute UTF-8"), "{error}");
                        assert!(!error.contains(f.root.to_str().unwrap()), "{error}");
                    }
                }
            }
            assert_eq!(
                tree(&f.root),
                before,
                "configured={configured}: no lock, state, binary or target write"
            );
        }
        assert_eq!(
            status(&f.root.join("b\u{7f}rain"), "work", ident(&f)),
            "unsafe_state"
        );
        unconfigure(&ctx(&f), "work", false).unwrap();
        assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes());
    }

    #[test]
    fn a_unicode_home_with_spaces_configures_and_restores() {
        let f = fixture_named("unicode", "дом ñ space");
        write_config(&f, ORIGINAL);
        assert_eq!(
            configure(&ctx(&f), "work", false).unwrap()["status"],
            "configured"
        );
        let text = String::from_utf8(config(&f).unwrap()).unwrap();
        let command = paths(&f.brain_home, "work").binary;
        assert!(command.is_absolute());
        let expected = crate::installer::toml_string(command.to_str().unwrap());
        assert!(text.contains(&expected));
        unconfigure(&ctx(&f), "work", false).unwrap();
        assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes());
    }

    #[test]
    fn doctor_uses_one_validated_codex_status_for_both_fields() {
        let f = fixture("doctor");
        let claude = f.home.join("claude-home");
        fs::create_dir(&claude).unwrap();
        private(&claude);
        client_registry::run(
            &ClientsCommand::Register {
                client: ClientKind::Claude,
                home: claude,
                label: "notes".into(),
                dry_run: false,
            },
            &f.home,
            &f.brain_home,
            1,
        )
        .unwrap();
        let homes = crate::client_home::classify_all(&f.home, None, None, None);
        let pair = |label: &str| {
            let report = client_registry::doctor_report(&f.home, &f.brain_home, &homes);
            let row = report["registrations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["label"] == label)
                .unwrap()
                .clone();
            (
                row["integration"].as_str().unwrap().to_string(),
                row["codex_mcp"].as_str().unwrap().to_string(),
            )
        };
        let same = |value: &str| (value.to_string(), value.to_string());
        assert_eq!(pair("work"), same("none"));
        write_config(&f, ORIGINAL);
        configure(&ctx(&f), "work", false).unwrap();
        // Doctor never opens the client's config.toml: unreadable, still configured.
        let target = f.codex.join(CONFIG_FILE);
        fs::set_permissions(&target, fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(pair("work"), same("configured"));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let p = paths(&f.brain_home, "work");
        let good = fs::read(&p.manifest).unwrap();
        fs::write(&p.manifest, b"{").unwrap();
        assert_eq!(pair("work"), same("unsafe_state"));
        assert_eq!(
            pair("notes"),
            ("not_implemented".to_string(), "not_applicable".to_string())
        );
        fs::write(&p.manifest, good).unwrap();
        unconfigure(&ctx(&f), "work", false).unwrap();
        assert_eq!(config(&f).unwrap(), ORIGINAL.as_bytes());
    }

    /// Directory handed to the child by the parent through the child's own
    /// environment (`Command::env`), never the parent's.
    const CHILD_DIR: &str = "BRAIN_R1_RUNNING_IMAGE_CHILD_DIR";

    fn wait_for(path: &Path, deadline: std::time::Instant) -> bool {
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// Child half of `running_image_survives_atomic_path_replacement`; it
    /// does nothing unless that test started it.
    #[test]
    #[ignore = "run only as the child of running_image_survives_atomic_path_replacement"]
    fn running_image_child() {
        let Some(dir) = std::env::var_os(CHILD_DIR).map(PathBuf::from) else {
            return;
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        fs::write(dir.join("ready"), b"").unwrap();
        assert!(
            wait_for(&dir.join("go"), deadline),
            "parent never replaced the path"
        );
        let proof = match read_source(&Source::RunningImage) {
            Ok((_, sha)) => sha,
            Err(error) => format!("error: {error}"),
        };
        let link = fs::read_link("/proc/self/exe")
            .map(|link| link.to_string_lossy().ends_with(" (deleted)"))
            .unwrap_or(false);
        fs::write(dir.join("proof"), format!("{proof}\n{link}\n")).unwrap();
    }

    /// S3 at runtime: the child runs an owned copy of this test executable;
    /// after it is running, the parent atomically renames different (never
    /// executed) ELF bytes over that path. `Source::RunningImage` in the child
    /// must still yield the bytes it is running, not what the path now holds.
    #[cfg(target_os = "linux")]
    #[test]
    fn running_image_survives_atomic_path_replacement() {
        let f = fixture("running-image");
        let dir = f.root.join("child");
        fs::create_dir(&dir).unwrap();
        private(&dir);
        let exe = dir.join("exe");
        let original = std::env::current_exe().unwrap();
        // An owned copy within the 128 MiB bound; the cargo output itself is
        // never modified.
        if fs::metadata(&original).unwrap().len() > MAX_BINARY_BYTES {
            let status = std::process::Command::new("strip")
                .arg("--strip-debug")
                .arg("-o")
                .arg(&exe)
                .arg(&original)
                .status()
                .expect("strip is needed to fit the test executable into 128 MiB");
            assert!(status.success(), "strip failed");
        } else {
            fs::copy(&original, &exe).unwrap();
        }
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
        let held = fs::read(&exe).unwrap();
        assert!(
            held.len() as u64 <= MAX_BINARY_BYTES,
            "the owned copy must fit the bound"
        );
        let held_sha = sha256(&held);
        drop(held);

        let mut child = std::process::Command::new(&exe)
            .args([
                "client_integration::tests::running_image_child",
                "--exact",
                "--ignored",
                "--test-threads=1",
            ])
            .env(CHILD_DIR, &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let finish = |child: &mut std::process::Child| loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        if !wait_for(&dir.join("ready"), deadline) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child never became ready");
        }
        // Atomic replacement of the path with different, safe ELF bytes.
        let replacement = elf(9);
        let temporary = dir.join("exe.new");
        fs::write(&temporary, &replacement).unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&temporary, &exe).unwrap();
        fs::write(dir.join("go"), b"").unwrap();
        let status = finish(&mut child).expect("the child did not finish within 30 s");
        assert!(status.success(), "child failed: {status:?}");

        let proof = fs::read_to_string(dir.join("proof")).unwrap();
        let mut lines = proof.lines();
        let running_sha = lines.next().unwrap();
        let deleted = lines.next().unwrap();
        assert_eq!(
            fs::read(&exe).unwrap(),
            replacement,
            "the path now names new bytes"
        );
        assert_ne!(sha256(&replacement), held_sha);
        assert_eq!(running_sha, held_sha, "RunningImage read the running image");
        assert_eq!(deleted, "true", "the child ran a replaced (unlinked) inode");
    }

    #[test]
    fn root_max_config_then_block_must_refuse_before_target_write() {
        let f = fixture("root-size");
        let original = "#".repeat(MAX_CONFIG_BYTES as usize);
        write_config(&f, &original);
        let result = configure(&ctx(&f), "work", false);
        let now = config(&f).unwrap();
        println!(
            "original={} after={} accepted={} unconfigure_refused={}",
            original.len(),
            now.len(),
            result.is_ok(),
            unconfigure(&ctx(&f), "work", false).is_err()
        );
        assert!(
            result.is_err(),
            "configured file exceeds read/recovery limit"
        );
        assert_eq!(now, original.as_bytes());
    }
    #[test]
    fn root_default_codex_symlink_alias_must_refuse() {
        let f = fixture("root-default-alias");
        write_config(&f, ORIGINAL);
        std::os::unix::fs::symlink(&f.codex, f.home.join(".codex")).unwrap();
        let before = config(&f).unwrap();
        let result = configure(&ctx(&f), "work", false);
        println!("default_symlink_alias_accepted={}", result.is_ok());
        assert!(
            result.is_err(),
            "registration is the default Codex home via symlink alias"
        );
        assert_eq!(config(&f).unwrap(), before);
    }
}
