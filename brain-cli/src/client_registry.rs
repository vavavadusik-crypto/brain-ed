//! Explicit, opt-in registry of agent-client homes (`brain clients …`).
//!
//! This is a foundation only: a registration records that the user says a
//! Codex, Claude Code or Hermes home lives in a given directory. Registration
//! alone never integrates with, discovers, follows or captures from that
//! directory; the only integration is the separate, explicit Codex MCP opt-in
//! in `client_integration` (`brain clients configure`). Environment variables
//! are only reported as hints, never used to select.
//!
//! Only the metadata of the registered directory itself (and symlink checks
//! of its path) is ever looked at: never its contents, auth, `.env`, config
//! or history. Paths are printed only by `brain clients list --show-paths`.
//! Checks and use are separate steps, so a process running as the same user
//! can still race them (TOCTOU); no sandbox is claimed.

use std::error::Error;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client_home::ClientHomes;

pub(crate) const REGISTRY_FILE: &str = "client-homes.json";
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;
const MAX_ENTRIES: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Generous on purpose: future registered homes add manifest entries.
const MAX_MANIFEST_ENTRIES: usize = 4096;
const LOCK_FILE: &str = "client-homes.lock";
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
const O_NOFOLLOW: i32 = 0o400000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ClientKind {
    Codex,
    Claude,
    Hermes,
}

impl ClientKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Hermes => "hermes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registration {
    label: String,
    client: ClientKind,
    home: String,
    identity: Identity,
    registered_at: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Registry {
    version: u32,
    homes: Vec<Registration>,
}

/// The registry as found. Unsafe and corrupt registries are reported, never
/// replaced or ignored in favour of anything else.
#[derive(Debug)]
pub(crate) enum RegistryState {
    Absent,
    Valid(Registry),
    Unsafe(&'static str),
    Corrupt(&'static str),
}

/// What the registered directory looks like now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HomeCheck {
    Ok,
    Missing,
    NotDirectory,
    Symlink,
    ForeignOwner,
    WritableByOthers,
    IdentityChanged,
    Uninspectable,
    InvalidPath,
}

impl HomeCheck {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::NotDirectory => "not_directory",
            Self::Symlink => "symlink_in_path",
            Self::ForeignOwner => "foreign_owner",
            Self::WritableByOthers => "writable_by_others",
            Self::IdentityChanged => "identity_changed",
            Self::Uninspectable => "uninspectable",
            Self::InvalidPath => "invalid_path",
        }
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum ClientsCommand {
    /// Register a client home explicitly (no integration or capture yet).
    Register {
        #[arg(long, value_enum)]
        client: ClientKind,
        /// Absolute path of the client's home directory.
        #[arg(long)]
        home: PathBuf,
        /// Name for this registration: a-z, 0-9 and '-', 1 to 32 characters.
        #[arg(long)]
        label: String,
        /// Validate and show the result without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// List registrations (labels and checks; paths only with --show-paths).
    List {
        #[arg(long)]
        show_paths: bool,
    },
    /// Add (or with `unconfigure`, remove) the Brain MCP entry in the
    /// `config.toml` of a registered Codex home. Exactly one of `--dry-run`
    /// or `--yes` is required. Configured is not a handshake or capture.
    Configure {
        #[arg(long)]
        label: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Remove the Brain MCP entry added by `configure`, restoring the exact
    /// original bytes when the file was not edited since.
    Unconfigure {
        #[arg(long)]
        label: String,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Remove a registration. The client's directory is never touched.
    Forget {
        #[arg(long)]
        label: String,
        #[arg(long)]
        dry_run: bool,
    },
}

type Failure = Box<dyn Error>;

fn fail(why: &str) -> Failure {
    why.to_string().into()
}

pub(crate) fn run(
    command: &ClientsCommand,
    home: &Path,
    brain_home: &Path,
    now: i64,
) -> Result<Value, Failure> {
    // Every subcommand, dry runs and `list` included, before any lock,
    // registry read or write. Configure/unconfigure keep their own guard too.
    crate::private_fs::require_safe_homes(home, brain_home)?;
    match command {
        ClientsCommand::Register {
            client,
            home: target,
            label,
            dry_run,
        } => register(home, brain_home, *client, target, label, *dry_run, now),
        ClientsCommand::List { show_paths } => list(home, brain_home, *show_paths),
        ClientsCommand::Forget { label, dry_run } => forget(brain_home, label, *dry_run),
        ClientsCommand::Configure {
            label,
            dry_run,
            yes,
        }
        | ClientsCommand::Unconfigure {
            label,
            dry_run,
            yes,
        } => {
            if dry_run == yes {
                return Err(fail("use exactly one of --dry-run or --yes"));
            }
            let ctx = crate::client_integration::Context {
                home,
                brain_home,
                source: crate::client_integration::Source::RunningImage,
            };
            if matches!(command, ClientsCommand::Configure { .. }) {
                crate::client_integration::configure(&ctx, label, *dry_run)
            } else {
                crate::client_integration::unconfigure(&ctx, label, *dry_run)
            }
        }
    }
}

/// A registered Codex home whose registry entry is valid and whose directory
/// check is `ok` right now. The default `~/.codex` (by path or inode) is
/// never one: it belongs to `brain init --agents`.
pub(crate) struct CodexHome {
    pub(crate) label: String,
    pub(crate) home: PathBuf,
    pub(crate) identity: Identity,
}

pub(crate) fn registered_codex(
    home: &Path,
    brain_home: &Path,
    label: &str,
) -> Result<CodexHome, Failure> {
    let Some(registry) = valid_registry(brain_home)? else {
        return Err(fail("refusing: no registry exists"));
    };
    let Some(entry) = registry.homes.iter().find(|entry| entry.label == label) else {
        return Err(fail("refusing: no registration has this label"));
    };
    if entry.client != ClientKind::Codex {
        return Err(fail("refusing: only Codex registrations can be configured"));
    }
    match check(home, entry) {
        HomeCheck::Ok => {}
        other => {
            return Err(format!(
                "refusing: the registered home is {}; nothing was changed",
                other.as_str()
            )
            .into());
        }
    }
    let default = home.join(".codex");
    let same_path = Path::new(&entry.home) == default;
    // `~/.codex` may itself be a symlink to the registered directory, so its
    // identity is taken from what it resolves to (metadata only). Anything
    // that cannot be resolved is a refusal, never "not an alias".
    let same_inode = match fs::symlink_metadata(&default) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(fail(
                "refusing: the default ~/.codex cannot be inspected; nothing was changed",
            ));
        }
        Ok(link) => match fs::metadata(&default) {
            Ok(resolved) => entry.identity.matches(&link) || entry.identity.matches(&resolved),
            Err(_) => {
                return Err(fail(
                    "refusing: the default ~/.codex cannot be resolved (for example a broken symlink); nothing was changed",
                ));
            }
        },
    };
    if same_path || same_inode {
        return Err(fail(
            "refusing: the default ~/.codex is managed by `brain init --agents`, not by registrations",
        ));
    }
    Ok(CodexHome {
        label: entry.label.clone(),
        home: PathBuf::from(&entry.home),
        identity: entry.identity,
    })
}

impl Identity {
    #[cfg(unix)]
    fn matches(&self, metadata: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.dev == metadata.dev() && self.ino == metadata.ino()
    }

    #[cfg(not(unix))]
    fn matches(&self, _metadata: &fs::Metadata) -> bool {
        false
    }
}

fn valid_label(label: &str) -> bool {
    (1..=32).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An absolute UTF-8 path without `.`/`..` or control characters, in its
/// normal form (repeated and trailing separators dropped).
fn normal_home(raw: &Path) -> Option<String> {
    let text = raw.to_str()?;
    // `Path::components` silently drops an embedded `.`; refuse it (and `..`)
    // in the raw text first. Repeated and trailing separators stay accepted.
    if text
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return None;
    }
    if !raw.is_absolute() || text.chars().any(char::is_control) {
        return None;
    }
    let mut normal = PathBuf::from("/");
    for component in raw.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(part) => normal.push(part),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    normal.to_str().map(str::to_string)
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    crate::private_fs::current_uid()
}

/// `Ok(false)` only for an exact `NotFound`; an existing Brain home must be a
/// real directory owned by this user with no group or other access.
fn brain_home_ready(brain_home: &Path) -> Result<bool, &'static str> {
    match fs::symlink_metadata(brain_home) {
        Ok(metadata) => {
            if metadata.file_type().is_dir() && owner_only(&metadata) {
                Ok(true)
            } else {
                Err("the Brain home is not a private directory owned by you")
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("the Brain home cannot be inspected"),
    }
}

#[cfg(unix)]
fn owner_only(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    current_uid() == Some(metadata.uid()) && metadata.mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn owner_only(_metadata: &fs::Metadata) -> bool {
    false
}

/// Regular, owner-only file read through one bounded no-follow handle whose
/// inode must be the one checked. `Ok(None)` only for an exact `NotFound`.
fn read_private(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || !owner_only(&metadata) {
        return Err(invalid("not a private regular file"));
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

pub(crate) fn load(brain_home: &Path) -> RegistryState {
    // A relative or control-character Brain home is the wrong location (for
    // example `./.brain` from an unset HOME): reported, never read.
    if !crate::private_fs::is_safe_absolute(brain_home) {
        return RegistryState::Unsafe(
            "the Brain home is not an absolute UTF-8 path without control characters",
        );
    }
    match brain_home_ready(brain_home) {
        Ok(true) => {}
        Ok(false) => return RegistryState::Absent,
        Err(why) => return RegistryState::Unsafe(why),
    }
    let bytes = match read_private(&brain_home.join(REGISTRY_FILE), MAX_REGISTRY_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return RegistryState::Absent,
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return RegistryState::Unsafe(
                "the registry is not a private regular file within the size limit",
            );
        }
        Err(_) => return RegistryState::Unsafe("the registry cannot be read safely"),
    };
    // Serde's message may quote field values, so it is never shown.
    let Ok(registry) = serde_json::from_slice::<Registry>(&bytes) else {
        return RegistryState::Corrupt("the registry is not valid schema version 1");
    };
    match validate(&registry) {
        Ok(()) => RegistryState::Valid(registry),
        Err(why) => RegistryState::Corrupt(why),
    }
}

fn validate(registry: &Registry) -> Result<(), &'static str> {
    if registry.version != 1 {
        return Err("the registry has an unsupported version");
    }
    if registry.homes.len() > MAX_ENTRIES {
        return Err("the registry has too many entries");
    }
    for (index, entry) in registry.homes.iter().enumerate() {
        if !valid_label(&entry.label) {
            return Err("the registry has an invalid label");
        }
        if normal_home(Path::new(&entry.home)).as_deref() != Some(entry.home.as_str()) {
            return Err("the registry has an invalid home");
        }
        let earlier = &registry.homes[..index];
        if earlier.iter().any(|other| other.label == entry.label) {
            return Err("the registry has a duplicate label");
        }
        if earlier
            .iter()
            .any(|other| other.home == entry.home || other.identity == entry.identity)
        {
            return Err("the registry has a duplicate home");
        }
    }
    Ok(())
}

/// No symlink on the way to `target`, target included, except in the
/// components that make up `HOME` itself (a documented alias).
fn symlink_free(home: &Path, target: &Path) -> HomeCheck {
    let mut current = PathBuf::new();
    for component in target.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if !(home.starts_with(&current) && current != target) {
                    return HomeCheck::Symlink;
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return HomeCheck::Missing,
            Err(_) => return HomeCheck::Uninspectable,
        }
    }
    HomeCheck::Ok
}

/// Metadata of the directory itself only; its contents are never opened.
fn inspect(home: &Path, target: &str) -> Result<Identity, HomeCheck> {
    let path = Path::new(target);
    match symlink_free(home, path) {
        HomeCheck::Ok => {}
        other => return Err(other),
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(HomeCheck::Missing),
        Err(_) => return Err(HomeCheck::Uninspectable),
    };
    if metadata.file_type().is_symlink() {
        return Err(HomeCheck::Symlink);
    }
    if !metadata.file_type().is_dir() {
        return Err(HomeCheck::NotDirectory);
    }
    identity(&metadata)
}

#[cfg(unix)]
fn identity(metadata: &fs::Metadata) -> Result<Identity, HomeCheck> {
    use std::os::unix::fs::MetadataExt;
    if current_uid() != Some(metadata.uid()) {
        return Err(HomeCheck::ForeignOwner);
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(HomeCheck::WritableByOthers);
    }
    Ok(Identity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
    })
}

#[cfg(not(unix))]
fn identity(_metadata: &fs::Metadata) -> Result<Identity, HomeCheck> {
    Err(HomeCheck::Uninspectable)
}

pub(crate) fn check(home: &Path, registration: &Registration) -> HomeCheck {
    if normal_home(Path::new(&registration.home)).as_deref() != Some(registration.home.as_str()) {
        return HomeCheck::InvalidPath;
    }
    match inspect(home, &registration.home) {
        Ok(found) if found == registration.identity => HomeCheck::Ok,
        Ok(_) => HomeCheck::IdentityChanged,
        Err(check) => check,
    }
}

fn valid_registry(brain_home: &Path) -> Result<Option<Registry>, Failure> {
    match load(brain_home) {
        RegistryState::Absent => Ok(None),
        RegistryState::Valid(registry) => Ok(Some(registry)),
        RegistryState::Unsafe(why) | RegistryState::Corrupt(why) => {
            Err(format!("refusing: {why}; nothing was changed").into())
        }
    }
}

fn save(brain_home: &Path, registry: &Registry) -> Result<(), Failure> {
    let bytes = serde_json::to_vec_pretty(registry)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(fail("refusing: the registry would exceed its size limit"));
    }
    if !brain_home_ready(brain_home).map_err(fail)? {
        crate::private_fs::ensure_private_home(brain_home)?;
        if !brain_home_ready(brain_home).map_err(fail)? {
            return Err(fail("the Brain home could not be created privately"));
        }
    }
    crate::private_fs::write_private(&brain_home.join(REGISTRY_FILE), &bytes)?;
    Ok(())
}

/// Exclusive, non-blocking `File::try_lock` (flock) on a persistent,
/// private, empty lock file held for a whole read-modify-write of the
/// registry. The file is never unlinked (unlinking would let two writers lock
/// different inodes). Release is an explicit unlock in `Drop`, so a descriptor
/// shared with a forked child cannot keep the registry locked.
pub(crate) struct RegistryLock {
    file: fs::File,
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

const UNSAFE_LOCK: &str =
    "refusing: the registry lock is not a private empty regular file; nothing was changed";

fn private_empty_file(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_file() && owner_only(metadata) && metadata.len() == 0
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

fn open_existing_lock(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK);
    }
    options.open(path)
}

pub(crate) fn acquire_lock(brain_home: &Path) -> Result<RegistryLock, Failure> {
    let path = brain_home.join(LOCK_FILE);
    let before = match fs::symlink_metadata(&path) {
        Ok(metadata) if private_empty_file(&metadata) => Some(metadata),
        Ok(_) => return Err(fail(UNSAFE_LOCK)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(fail(UNSAFE_LOCK)),
    };
    let file = if before.is_some() {
        open_existing_lock(&path).map_err(|_| fail(UNSAFE_LOCK))?
    } else {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
            options.custom_flags(O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                open_existing_lock(&path).map_err(|_| fail(UNSAFE_LOCK))?
            }
            Err(_) => return Err(fail(UNSAFE_LOCK)),
        }
    };
    let opened = file.metadata().map_err(|_| fail(UNSAFE_LOCK))?;
    let now = fs::symlink_metadata(&path).map_err(|_| fail(UNSAFE_LOCK))?;
    if !private_empty_file(&opened)
        || !same_inode(&opened, &now)
        || before
            .as_ref()
            .is_some_and(|before| !same_inode(before, &opened))
    {
        return Err(fail(UNSAFE_LOCK));
    }
    match file.try_lock() {
        Ok(()) => Ok(RegistryLock { file }),
        Err(fs::TryLockError::WouldBlock) => Err(fail(
            "refusing: another registry change is in progress; nothing was changed, try again",
        )),
        Err(fs::TryLockError::Error(_)) => Err(fail(UNSAFE_LOCK)),
    }
}

/// Creates the Brain home when it is missing (real writes only) and requires
/// it to be private.
fn prepare_brain_home(brain_home: &Path) -> Result<(), Failure> {
    if !brain_home_ready(brain_home).map_err(fail)? {
        crate::private_fs::ensure_private_home(brain_home)?;
        if !brain_home_ready(brain_home).map_err(fail)? {
            return Err(fail("the Brain home could not be created privately"));
        }
    }
    Ok(())
}

fn admit(registry: &Registry, label: &str, home: &str, identity: Identity) -> Result<(), Failure> {
    if registry.homes.iter().any(|entry| entry.label == label) {
        return Err(fail("refusing: this label is already registered"));
    }
    if registry
        .homes
        .iter()
        .any(|entry| entry.home == home || entry.identity == identity)
    {
        return Err(fail("refusing: this directory is already registered"));
    }
    if registry.homes.len() >= MAX_ENTRIES {
        return Err(fail("refusing: the registry already holds 64 entries"));
    }
    Ok(())
}

fn empty_registry() -> Registry {
    Registry {
        version: 1,
        homes: Vec::new(),
    }
}

fn register(
    home: &Path,
    brain_home: &Path,
    client: ClientKind,
    target: &Path,
    label: &str,
    dry_run: bool,
    now: i64,
) -> Result<Value, Failure> {
    if !valid_label(label) {
        return Err(fail(
            "invalid label: use a-z, 0-9 and '-', 1 to 32 characters",
        ));
    }
    let Some(normal) = normal_home(target) else {
        return Err(fail(
            "invalid home: it must be an absolute UTF-8 path without '.' or '..'",
        ));
    };
    let identity = inspect(home, &normal).map_err(|check| {
        format!(
            "refusing to register: the home is {}; nothing was changed",
            check.as_str()
        )
    })?;
    if dry_run {
        // A read-only snapshot check: no lock, no Brain home, nothing reserved.
        let registry = valid_registry(brain_home)?.unwrap_or_else(empty_registry);
        admit(&registry, label, &normal, identity)?;
    } else {
        prepare_brain_home(brain_home)?;
        let _lock = acquire_lock(brain_home)?;
        let mut registry = valid_registry(brain_home)?.unwrap_or_else(empty_registry);
        admit(&registry, label, &normal, identity)?;
        registry.homes.push(Registration {
            label: label.to_string(),
            client,
            home: normal,
            identity,
            registered_at: now,
        });
        save(brain_home, &registry)?;
    }
    Ok(json!({
        "status": if dry_run { "dry_run" } else { "registered" },
        "label": label,
        "client": client.as_str(),
        "files_written": if dry_run { 0 } else { 1 },
        "integration": "none: registration only, no client configuration or capture",
    }))
}

fn list(home: &Path, brain_home: &Path, show_paths: bool) -> Result<Value, Failure> {
    let registry = valid_registry(brain_home)?;
    let entries = registry
        .iter()
        .flat_map(|registry| &registry.homes)
        .map(|entry| {
            let mut row = json!({
                "label": entry.label,
                "client": entry.client.as_str(),
                "check": check(home, entry).as_str(),
            });
            if show_paths {
                row["home"] = Value::String(entry.home.clone());
            }
            row
        })
        .collect::<Vec<_>>();
    Ok(json!({"registry": if registry.is_some() { "valid" } else { "absent" }, "homes": entries}))
}

/// Mirror of the installer's manifest schema v1 (`installer.rs`), checked
/// strictly. Paths are any absolute canonical path, so manifests that later
/// record registered homes stay representable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallManifest {
    version: u32,
    entries: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    path: PathBuf,
    existed: bool,
    before_sha256: Option<String>,
    after_sha256: String,
    backup: Option<PathBuf>,
    mode: Option<u32>,
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_backup_name(backup: &Path) -> bool {
    backup.to_str().is_some_and(|name| {
        name.len() == 7 && name.ends_with(".bak") && name[..3].bytes().all(|b| b.is_ascii_digit())
    })
}

fn valid_manifest(manifest: &InstallManifest) -> bool {
    if manifest.version != 1 || manifest.entries.len() > MAX_MANIFEST_ENTRIES {
        return false;
    }
    let mut paths: Vec<&Path> = Vec::new();
    let mut backups: Vec<&Path> = Vec::new();
    for entry in &manifest.entries {
        let canonical = entry
            .path
            .to_str()
            .is_some_and(|text| normal_home(&entry.path).as_deref() == Some(text));
        if !canonical || paths.contains(&entry.path.as_path()) || !is_sha256(&entry.after_sha256) {
            return false;
        }
        paths.push(&entry.path);
        match (entry.existed, &entry.before_sha256, &entry.backup) {
            (true, Some(before), Some(backup)) => {
                if !is_sha256(before)
                    || !valid_backup_name(backup)
                    || backups.contains(&backup.as_path())
                {
                    return false;
                }
                backups.push(backup);
            }
            (false, None, None) => {}
            _ => return false,
        }
        if let Some(mode) = entry.mode {
            let kind = mode & !0o7777;
            if (kind != 0 && kind != 0o100000) || mode & 0o7000 != 0 {
                return false;
            }
        }
    }
    true
}

/// Paths recorded by `brain init --agents`. Anything that is not a bounded,
/// private, strictly valid v1 manifest is a reason to refuse. Only the
/// manifest is read: never client files or backups.
fn manifest_paths(brain_home: &Path) -> Result<Vec<PathBuf>, &'static str> {
    let unsafe_manifest = "the install manifest cannot be checked safely";
    let bytes = match read_private(
        &brain_home.join("install-manifest.json"),
        MAX_MANIFEST_BYTES,
    ) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return Ok(Vec::new()),
        Err(_) => return Err(unsafe_manifest),
    };
    let manifest: InstallManifest = serde_json::from_slice(&bytes).map_err(|_| unsafe_manifest)?;
    if !valid_manifest(&manifest) {
        return Err(unsafe_manifest);
    }
    Ok(manifest
        .entries
        .into_iter()
        .map(|entry| entry.path)
        .collect())
}

fn forget(brain_home: &Path, label: &str, dry_run: bool) -> Result<Value, Failure> {
    // A real forget holds the lock across the whole read-modify-write; a dry
    // run is a read-only snapshot and creates nothing.
    let _lock = if dry_run {
        None
    } else {
        match brain_home_ready(brain_home).map_err(fail)? {
            true => Some(acquire_lock(brain_home)?),
            false => return Err(fail("refusing: no registry exists")),
        }
    };
    let Some(mut registry) = valid_registry(brain_home)? else {
        return Err(fail("refusing: no registry exists"));
    };
    let Some(index) = registry.homes.iter().position(|entry| entry.label == label) else {
        return Err(fail("refusing: no registration has this label"));
    };
    if let Some(why) = crate::client_integration::forget_blocker(brain_home, label) {
        return Err(format!("refusing to forget: {why}; nothing was changed").into());
    }
    let home = PathBuf::from(&registry.homes[index].home);
    let recorded = manifest_paths(brain_home)
        .map_err(|why| format!("refusing to forget: {why}; nothing was changed"))?;
    if recorded.iter().any(|path| path.starts_with(&home)) {
        return Err(fail(
            "refusing to forget: an installation recorded under this home still exists; run `brain uninstall` first",
        ));
    }
    registry.homes.remove(index);
    if !dry_run {
        save(brain_home, &registry)?;
    }
    Ok(json!({
        "status": if dry_run { "dry_run" } else { "forgotten" },
        "label": label,
        "files_written": if dry_run { 0 } else { 1 },
        "client_directory": "not touched",
    }))
}

/// Additive `brain doctor` section: registry state and per-registration
/// label, client, check and environment hint. Never a path, never a value.
pub(crate) fn doctor_report(home: &Path, brain_home: &Path, homes: &ClientHomes) -> Value {
    // Rows are checked against `home`, so an unusable `home` makes the whole
    // section unsafe before the registry is read; `load` guards `brain_home`.
    let registry_state = if crate::private_fs::is_safe_absolute(home) {
        load(brain_home)
    } else {
        RegistryState::Unsafe("HOME is not an absolute UTF-8 path without control characters")
    };
    let (state, reason, registry) = match registry_state {
        RegistryState::Absent => ("absent", None, None),
        RegistryState::Valid(registry) => ("valid", None, Some(registry)),
        RegistryState::Unsafe(why) => ("unsafe", Some(why), None),
        RegistryState::Corrupt(why) => ("corrupt", Some(why), None),
    };
    let registrations = registry
        .iter()
        .flat_map(|registry| &registry.homes)
        .map(|entry| {
            let hint = match entry.client {
                ClientKind::Codex => homes.codex,
                ClientKind::Claude => homes.claude,
                ClientKind::Hermes => homes.hermes,
            };
            // Codex rows have exactly one integration (the opt-in MCP entry),
            // so its validated status is computed once and reported in both
            // fields; other clients have no integration yet.
            let (integration, codex_mcp) = if entry.client == ClientKind::Codex {
                let status =
                    crate::client_integration::status(brain_home, &entry.label, entry.identity);
                (status, status)
            } else {
                ("not_implemented", "not_applicable")
            };
            json!({
                "label": entry.label,
                "client": entry.client.as_str(),
                "check": check(home, entry).as_str(),
                "integration": integration,
                "codex_mcp": codex_mcp,
                "env_hint": hint.location(),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "state": state,
        "reason": reason,
        "count": registrations.len(),
        "registrations": registrations,
        "selection": "explicit registrations only; environment variables are hints",
    })
}

#[cfg(all(test, unix))]
mod home_guard_tests {
    use super::*;
    use crate::private_fs::UNSAFE_HOME;
    use crate::private_fs::tests::{snapshot, unsafe_home_pairs};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn every_clients_command_refuses_unusable_homes_before_any_io() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-registry-guard-{}-{nonce}",
            std::process::id()
        ));
        // A healthy registration a loose guard could read, lock or rewrite.
        let home = root.join("ok");
        let client = home.join("work-codex");
        fs::create_dir_all(&client).unwrap();
        for dir in [&root, &home, &client] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let brain_home = home.join(".brain");
        let register = |dry_run| ClientsCommand::Register {
            client: ClientKind::Codex,
            home: client.clone(),
            label: "work".into(),
            dry_run,
        };
        run(&register(false), &home, &brain_home, 1).unwrap();
        let before = snapshot(&root);
        let commands = [
            register(true),
            register(false),
            ClientsCommand::List { show_paths: true },
            ClientsCommand::Forget {
                label: "work".into(),
                dry_run: true,
            },
            ClientsCommand::Forget {
                label: "work".into(),
                dry_run: false,
            },
            ClientsCommand::Configure {
                label: "work".into(),
                dry_run: true,
                yes: false,
            },
            ClientsCommand::Unconfigure {
                label: "work".into(),
                dry_run: false,
                yes: true,
            },
        ];
        for (unsafe_home, unsafe_brain_home) in unsafe_home_pairs(&root) {
            for command in &commands {
                let error = run(command, &unsafe_home, &unsafe_brain_home, 1)
                    .unwrap_err()
                    .to_string();
                assert_eq!(error, UNSAFE_HOME, "{unsafe_home:?} {command:?}");
            }
            let hints = crate::client_home::classify_all(&home, None, None, None);
            let report = doctor_report(&unsafe_home, &unsafe_brain_home, &hints);
            assert_eq!(report["state"], "unsafe");
            assert_eq!(report["count"], 0);
        }
        assert_eq!(
            snapshot(&root),
            before,
            "no lock, registry or client change"
        );
        // The healthy absolute context still works afterwards.
        let listed = run(
            &ClientsCommand::List { show_paths: false },
            &home,
            &brain_home,
            1,
        );
        assert_eq!(listed.unwrap()["homes"][0]["check"], "ok");
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "brain-registry-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    /// A synthetic HOME with one owner-only client directory inside it.
    fn fixture(label: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = scratch(label);
        let home = root.join("home");
        let client = home.join("work-codex");
        fs::create_dir_all(&client).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
        // Contents that must never be read.
        fs::write(client.join("auth.json"), b"SECRET-TOKEN").unwrap();
        fs::set_permissions(client.join("auth.json"), fs::Permissions::from_mode(0o000)).unwrap();
        let brain_home = home.join(".brain");
        (root, home, brain_home, client)
    }

    fn reg(
        home: &Path,
        brain_home: &Path,
        target: &Path,
        label: &str,
        dry_run: bool,
    ) -> Result<Value, Failure> {
        register(
            home,
            brain_home,
            ClientKind::Codex,
            target,
            label,
            dry_run,
            1_760_000_000,
        )
    }

    fn unset(home: &Path) -> ClientHomes {
        crate::client_home::classify_all(home, None, None, None)
    }

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// A manifest in the installer's real v1 shape: one created file and,
    /// for every further path, one existing file with a backup.
    fn manifest_json(paths: &[PathBuf]) -> Value {
        let sha = "a".repeat(64);
        let entries = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                if index == 0 {
                    json!({"path": path, "existed": false, "before_sha256": null,
                        "after_sha256": sha, "backup": null, "mode": null})
                } else {
                    json!({"path": path, "existed": true, "before_sha256": "b".repeat(64),
                        "after_sha256": sha, "backup": format!("{index:03}.bak"), "mode": 0o100644})
                }
            })
            .collect::<Vec<_>>();
        json!({"version": 1, "entries": entries})
    }

    #[test]
    fn strict_manifest_schema_or_refusal_before_forget() {
        let (root, home, brain_home, client) = fixture("manifest");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let path = brain_home.join(REGISTRY_FILE);
        let before = fs::read(&path).unwrap();
        let manifest = brain_home.join("install-manifest.json");
        let elsewhere = [home.join(".codex/config.toml"), home.join(".claude.json")];
        let valid = manifest_json(&elsewhere);
        let edit = |change: &dyn Fn(&mut Value)| {
            let mut value = valid.clone();
            change(&mut value);
            value
        };
        let invalid = vec![
            (
                "root finding",
                json!({"version": 999, "entries": [], "unknown": "synthetic"}),
            ),
            ("version", edit(&|m| m["version"] = 2.into())),
            ("unknown field", edit(&|m| m["extra"] = true.into())),
            (
                "unknown entry field",
                edit(&|m| m["entries"][0]["client"] = "x".into()),
            ),
            ("entries type", json!({"version": 1, "entries": {}})),
            (
                "relative path",
                edit(&|m| m["entries"][0]["path"] = "rel/x".into()),
            ),
            (
                "dot path",
                edit(&|m| m["entries"][0]["path"] = "/a/./b".into()),
            ),
            (
                "parent path",
                edit(&|m| m["entries"][0]["path"] = "/a/../b".into()),
            ),
            (
                "noncanonical path",
                edit(&|m| m["entries"][0]["path"] = "/a//b".into()),
            ),
            (
                "duplicate path",
                edit(&|m| m["entries"][1]["path"] = m["entries"][0]["path"].clone()),
            ),
            (
                "bad sha",
                edit(&|m| m["entries"][0]["after_sha256"] = "zz".into()),
            ),
            (
                "existed mismatch",
                edit(&|m| m["entries"][1]["backup"] = Value::Null),
            ),
            (
                "backup name",
                edit(&|m| m["entries"][1]["backup"] = "../x".into()),
            ),
            (
                "setuid mode",
                edit(&|m| m["entries"][1]["mode"] = 0o104755.into()),
            ),
            ("path type", edit(&|m| m["entries"][0]["path"] = 7.into())),
        ];
        for (label, value) in invalid {
            crate::private_fs::write_private(&manifest, value.to_string().as_bytes()).unwrap();
            let error = forget(&brain_home, "work", false).unwrap_err().to_string();
            assert!(
                error.contains("cannot be checked safely"),
                "{label}: {error}"
            );
            assert!(!error.contains("synthetic"), "{label}");
            assert!(
                forget(&brain_home, "work", true).is_err(),
                "{label}: dry run too"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "{label}: registry unchanged"
            );
        }
        // The real installer shape with gaps in backup numbering is accepted.
        crate::private_fs::write_private(&manifest, valid.to_string().as_bytes()).unwrap();
        assert_eq!(
            forget(&brain_home, "work", false).unwrap()["status"],
            "forgotten"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn raw_dot_segments_are_refused_but_repeated_slashes_are_canonical() {
        let (root, home, brain_home, client) = fixture("dots");
        let text = client.to_str().unwrap().to_string();
        let parent = client.parent().unwrap().to_str().unwrap().to_string();
        for raw in [
            format!("{parent}/./work-codex"),
            format!("{text}/."),
            format!("{parent}/../home/work-codex"),
            format!("{text}/.."),
        ] {
            let error = reg(&home, &brain_home, Path::new(&raw), "work", true)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("invalid home") && !error.contains(&text),
                "{raw}: {error}"
            );
        }
        assert!(!brain_home.exists());
        let spelled = format!("{}//{}//", parent.replace('/', "//"), "work-codex");
        reg(&home, &brain_home, Path::new(&spelled), "work", false).unwrap();
        let listed = list(&home, &brain_home, true).unwrap();
        assert_eq!(listed["homes"][0]["home"], text.as_str());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dry_runs_list_and_doctor_never_create_the_lock() {
        let (root, home, brain_home, client) = fixture("nolock");
        fs::create_dir(&brain_home).unwrap();
        fs::set_permissions(&brain_home, fs::Permissions::from_mode(0o700)).unwrap();
        reg(&home, &brain_home, &client, "work", true).unwrap();
        let _ = list(&home, &brain_home, false).unwrap();
        let _ = doctor_report(&home, &brain_home, &unset(&home));
        assert_eq!(
            fs::read_dir(&brain_home).unwrap().count(),
            0,
            "nothing created"
        );
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let lock = brain_home.join(LOCK_FILE);
        assert_eq!(fs::symlink_metadata(&lock).unwrap().len(), 0);
        assert_eq!(mode(&lock), 0o600);
        forget(&brain_home, "work", false).unwrap();
        assert!(lock.exists(), "the live lock is never unlinked");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_lock_files_are_refused_without_changes() {
        let (root, home, brain_home, client) = fixture("badlock");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let path = brain_home.join(REGISTRY_FILE);
        let before = fs::read(&path).unwrap();
        let lock = brain_home.join(LOCK_FILE);
        let other = home.join("other");
        fs::create_dir(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
        let target = root.join("lock-target");
        let check = |label: &str| {
            let error = reg(&home, &brain_home, &other, "other", false)
                .unwrap_err()
                .to_string();
            assert!(error.contains("registry lock"), "{label}: {error}");
            assert!(forget(&brain_home, "work", false).is_err(), "{label}");
            assert_eq!(fs::read(&path).unwrap(), before, "{label}");
        };
        fs::remove_file(&lock).unwrap();
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &lock).unwrap();
        check("symlink");
        fs::remove_file(&lock).unwrap();
        fs::create_dir(&lock).unwrap();
        check("directory");
        fs::remove_dir(&lock).unwrap();
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        check("permissive");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&lock, b"12345").unwrap();
        check("not empty");
        fs::write(&lock, b"").unwrap();
        if current_uid() == Some(0) {
            std::os::unix::fs::chown(&lock, Some(12345), None).unwrap();
            check("foreign owner");
            std::os::unix::fs::chown(&lock, Some(0), None).unwrap();
        }
        assert_eq!(
            fs::read(&target).unwrap(),
            b"",
            "a lock target is never written"
        );
        reg(&home, &brain_home, &other, "other", false).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_held_lock_refuses_and_its_release_admits() {
        let (root, home, brain_home, client) = fixture("busy");
        prepare_brain_home(&brain_home).unwrap();
        let held = acquire_lock(&brain_home).unwrap();
        let error = reg(&home, &brain_home, &client, "work", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("in progress"), "{error}");
        assert!(!brain_home.join(REGISTRY_FILE).exists());
        drop(held);
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    fn busy(error: &str) -> bool {
        error.contains("in progress")
    }

    #[test]
    fn concurrent_registrations_never_lose_a_successful_write() {
        let (root, home, brain_home, _) = fixture("race");
        let dirs = (0..32)
            .map(|index| {
                let dir = home.join(format!("c{index}"));
                fs::create_dir(&dir).unwrap();
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
                dir
            })
            .collect::<Vec<_>>();
        // One attempt each: busy refusals are allowed, lost successes are not.
        let once = std::thread::scope(|scope| {
            let handles = dirs[..16]
                .iter()
                .enumerate()
                .map(|(i, dir)| {
                    let (home, brain_home) = (&home, &brain_home);
                    scope.spawn(move || {
                        reg(home, brain_home, dir, &format!("c{i}"), false)
                            .map(|_| format!("c{i}"))
                            .map_err(|e| e.to_string())
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut succeeded = once
            .iter()
            .filter_map(|r| r.clone().ok())
            .collect::<Vec<_>>();
        assert!(
            once.iter()
                .all(|r| r.as_ref().map_or_else(|e| busy(e), |_| true)),
            "{once:?}"
        );
        let stored = |brain_home: &Path| {
            let mut labels = list(&home, brain_home, false).unwrap()["homes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["label"].as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            labels.sort();
            labels
        };
        succeeded.sort();
        assert_eq!(stored(&brain_home), succeeded, "every success is stored");
        // Retrying on busy: all 32 end up stored.
        std::thread::scope(|scope| {
            for (i, dir) in dirs.iter().enumerate() {
                let (home, brain_home) = (&home, &brain_home);
                scope.spawn(move || {
                    let label = format!("c{i}");
                    for _ in 0..100_000 {
                        match reg(home, brain_home, dir, &label, false) {
                            Ok(_) => return,
                            Err(e) if e.to_string().contains("already registered") => return,
                            Err(e) if busy(&e.to_string()) => std::thread::yield_now(),
                            Err(e) => panic!("{label}: {e}"),
                        }
                    }
                    panic!("{label}: never admitted");
                });
            }
        });
        let mut all = (0..32).map(|i| format!("c{i}")).collect::<Vec<_>>();
        all.sort();
        assert_eq!(stored(&brain_home), all);
        // Concurrent forget and register: no resurrection, no lost success.
        std::thread::scope(|scope| {
            for i in 0..32 {
                let (home, brain_home, dirs) = (&home, &brain_home, &dirs);
                scope.spawn(move || {
                    for _ in 0..100_000 {
                        let result = if i % 2 == 0 {
                            forget(brain_home, &format!("c{i}"), false).map(|_| ())
                        } else {
                            reg(home, brain_home, &dirs[i], &format!("c{i}"), false).map(|_| ())
                        };
                        match result {
                            Ok(()) => return,
                            Err(e) if e.to_string().contains("already registered") => return,
                            Err(e) if busy(&e.to_string()) => std::thread::yield_now(),
                            Err(e) => panic!("{i}: {e}"),
                        }
                    }
                });
            }
        });
        let mut odd = (0..32)
            .filter(|i| i % 2 == 1)
            .map(|i| format!("c{i}"))
            .collect::<Vec<_>>();
        odd.sort();
        assert_eq!(
            stored(&brain_home),
            odd,
            "forgotten stay forgotten, kept stay kept"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_writes_nothing_not_even_the_brain_home() {
        let (root, home, brain_home, client) = fixture("dry");
        let result = reg(&home, &brain_home, &client, "work", true).unwrap();
        assert_eq!(result["status"], "dry_run");
        assert_eq!(result["files_written"], 0);
        assert!(
            !brain_home.exists(),
            "no Brain home is created by a dry run"
        );
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let before = fs::read(brain_home.join(REGISTRY_FILE)).unwrap();
        let other = home.join("other");
        fs::create_dir(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
        reg(&home, &brain_home, &other, "other", true).unwrap();
        forget(&brain_home, "work", true).unwrap();
        assert_eq!(fs::read(brain_home.join(REGISTRY_FILE)).unwrap(), before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn register_is_private_and_never_prints_paths() {
        let (root, home, brain_home, client) = fixture("private");
        let result = reg(&home, &brain_home, &client, "work", false).unwrap();
        assert_eq!(mode(&brain_home), 0o700);
        assert_eq!(mode(&brain_home.join(REGISTRY_FILE)), 0o600);
        let path = client.to_str().unwrap();
        let listed = list(&home, &brain_home, false).unwrap();
        let doctor = doctor_report(&home, &brain_home, &unset(&home));
        for text in [result.to_string(), listed.to_string(), doctor.to_string()] {
            assert!(!text.contains(path) && !text.contains("SECRET"), "{text}");
        }
        assert_eq!(listed["homes"][0]["check"], "ok");
        assert_eq!(doctor["registrations"][0]["check"], "ok");
        // A registered but unconfigured Codex home: one status, both fields.
        assert_eq!(doctor["registrations"][0]["integration"], "none");
        assert_eq!(doctor["registrations"][0]["codex_mcp"], "none");
        let shown = list(&home, &brain_home, true).unwrap();
        assert_eq!(shown["homes"][0]["home"], path);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unsafe_or_invalid_homes_are_refused_without_writes() {
        let (root, home, brain_home, client) = fixture("unsafe");
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = home.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let below_link = real.join("child");
        fs::create_dir(&below_link).unwrap();
        fs::set_permissions(&below_link, fs::Permissions::from_mode(0o700)).unwrap();
        let open = home.join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        let file = home.join("file");
        fs::write(&file, b"x").unwrap();
        let non_utf8 = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/brain-\xff"));
        let cases: Vec<PathBuf> = vec![
            link.clone(),
            link.join("child"),
            open,
            file,
            home.join("missing"),
            PathBuf::from("relative/dir"),
            home.join("work-codex/../work-codex"),
            non_utf8,
        ];
        for target in cases {
            let error = reg(&home, &brain_home, &target, "work", false)
                .unwrap_err()
                .to_string();
            assert!(
                !error.contains(home.to_str().unwrap()),
                "no path in: {error}"
            );
            assert!(!brain_home.exists(), "{target:?}: nothing written");
        }
        let _ = client;
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn home_itself_may_be_an_alias() {
        let root = scratch("alias");
        let real_home = root.join("real-home");
        let client = real_home.join("codex");
        fs::create_dir_all(&client).unwrap();
        fs::set_permissions(&real_home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("home");
        std::os::unix::fs::symlink(&real_home, &home).unwrap();
        let brain_home = real_home.join(".brain");
        reg(&home, &brain_home, &home.join("codex"), "work", false).unwrap();
        assert_eq!(
            list(&home, &brain_home, false).unwrap()["homes"][0]["check"],
            "ok"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicates_labels_and_limits_are_refused() {
        let (root, home, brain_home, client) = fixture("dupes");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let before = fs::read(brain_home.join(REGISTRY_FILE)).unwrap();
        let other = home.join("other");
        fs::create_dir(&other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
        let spelled = PathBuf::from(format!("{}//", client.display()));
        for (target, label) in [
            (other.clone(), "work"),
            (client.clone(), "again"),
            (spelled, "spelled"),
            (other.clone(), "Upper"),
            (other.clone(), ""),
            (other.clone(), "a-very-long-label-that-is-over-32"),
        ] {
            assert!(
                reg(&home, &brain_home, &target, label, false).is_err(),
                "{label}"
            );
        }
        assert_eq!(fs::read(brain_home.join(REGISTRY_FILE)).unwrap(), before);
        for index in 1..MAX_ENTRIES {
            let dir = home.join(format!("d{index}"));
            fs::create_dir(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            reg(&home, &brain_home, &dir, &format!("d{index}"), false).unwrap();
        }
        assert!(reg(&home, &brain_home, &other, "one-too-many", false).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_or_unsafe_registries_are_reported_and_refused() {
        let (root, home, brain_home, client) = fixture("corrupt");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let path = brain_home.join(REGISTRY_FILE);
        let good = fs::read(&path).unwrap();
        let secret_value = "/private/value-that-must-not-leak";
        let unknown = String::from_utf8(good.clone()).unwrap().replacen(
            "\"version\"",
            &format!("\"extra\": \"{secret_value}\", \"version\""),
            1,
        );
        let cases: Vec<(&str, Vec<u8>, u32, &str)> = vec![
            ("bad json", b"{not json".to_vec(), 0o600, "corrupt"),
            ("unknown field", unknown.into_bytes(), 0o600, "corrupt"),
            (
                "oversize",
                vec![b' '; MAX_REGISTRY_BYTES as usize + 1],
                0o600,
                "unsafe",
            ),
            ("permissive", good.clone(), 0o644, "unsafe"),
        ];
        for (label, bytes, file_mode, state) in cases {
            fs::write(&path, &bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(file_mode)).unwrap();
            let report = doctor_report(&home, &brain_home, &unset(&home));
            assert_eq!(report["state"], state, "{label}");
            assert!(!report.to_string().contains(secret_value), "{label}");
            let other = home.join(format!("x-{}", label.replace(' ', "-")));
            fs::create_dir(&other).unwrap();
            fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
            let error = reg(&home, &brain_home, &other, "new", false)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("refusing") && !error.contains(secret_value),
                "{error}"
            );
            assert!(list(&home, &brain_home, true).is_err());
            assert!(forget(&brain_home, "work", false).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes, "{label}: no rewrite");
        }
        fs::remove_file(&path).unwrap();
        let elsewhere = root.join("elsewhere.json");
        fs::write(&elsewhere, &good).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert_eq!(
            doctor_report(&home, &brain_home, &unset(&home))["state"],
            "unsafe"
        );
        fs::remove_file(&path).unwrap();
        fs::set_permissions(&brain_home, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            doctor_report(&home, &brain_home, &unset(&home))["state"],
            "unsafe"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_replaced_or_loosened_directory_is_reported_and_not_ok() {
        let (root, home, brain_home, client) = fixture("identity");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        fs::rename(&client, root.join("moved")).unwrap();
        assert_eq!(
            list(&home, &brain_home, false).unwrap()["homes"][0]["check"],
            "missing"
        );
        fs::create_dir(&client).unwrap();
        fs::set_permissions(&client, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            list(&home, &brain_home, false).unwrap()["homes"][0]["check"],
            "identity_changed"
        );
        fs::set_permissions(&client, fs::Permissions::from_mode(0o770)).unwrap();
        assert_eq!(
            list(&home, &brain_home, false).unwrap()["homes"][0]["check"],
            "writable_by_others"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn forget_refuses_while_an_installation_is_recorded_under_the_home() {
        let (root, home, brain_home, client) = fixture("forget");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let manifest = brain_home.join("install-manifest.json");
        let recorded = manifest_json(&[client.join("config.toml")]);
        crate::private_fs::write_private(&manifest, recorded.to_string().as_bytes()).unwrap();
        let before = fs::read(brain_home.join(REGISTRY_FILE)).unwrap();
        let error = forget(&brain_home, "work", false).unwrap_err().to_string();
        assert!(error.contains("brain uninstall") && !error.contains(client.to_str().unwrap()));
        assert_eq!(fs::read(brain_home.join(REGISTRY_FILE)).unwrap(), before);
        // An invalid or unsafe manifest is a refusal too.
        crate::private_fs::write_private(&manifest, b"{\"entries\": 3}").unwrap();
        assert!(forget(&brain_home, "work", false).is_err());
        fs::set_permissions(&manifest, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(forget(&brain_home, "work", false).is_err());
        fs::remove_file(&manifest).unwrap();
        let other = manifest_json(&[home.join(".codex/config.toml")]);
        crate::private_fs::write_private(&manifest, other.to_string().as_bytes()).unwrap();
        let result = forget(&brain_home, "work", false).unwrap();
        assert_eq!(result["status"], "forgotten");
        assert_eq!(list(&home, &brain_home, false).unwrap()["homes"], json!([]));
        assert!(
            client.join("auth.json").exists(),
            "the client directory is untouched"
        );
        assert_eq!(mode(&client.join("auth.json")), 0o000);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn absent_registry_is_the_old_behaviour() {
        let root = scratch("absent");
        let home = root.join("home");
        let brain_home = home.join(".brain");
        let report = doctor_report(&home, &brain_home, &unset(&home));
        assert_eq!(report["state"], "absent");
        assert_eq!(report["count"], 0);
        assert_eq!(
            list(&home, &brain_home, false).unwrap()["registry"],
            "absent"
        );
        assert!(forget(&brain_home, "work", false).is_err());
        assert!(!brain_home.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn environment_variables_are_hints_only() {
        let (root, home, brain_home, client) = fixture("hint");
        reg(&home, &brain_home, &client, "work", false).unwrap();
        let homes = crate::client_home::classify_all(&home, None, Some(client.as_os_str()), None);
        let report = doctor_report(&home, &brain_home, &homes);
        assert_eq!(
            report["registrations"][0]["env_hint"]["variable"],
            "CODEX_HOME"
        );
        assert_eq!(
            report["registrations"][0]["env_hint"]["state"],
            "unsupported_override"
        );
        assert!(!report.to_string().contains(client.to_str().unwrap()));
        let _ = fs::remove_dir_all(root);
    }
}
