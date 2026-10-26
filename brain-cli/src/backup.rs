//! `brain backup`: consistent snapshots of brain.sqlite and the conversation
//! archive with rotation and restore.
//!
//! Snapshots are taken with `VACUUM INTO`, not by copying files: the live
//! database runs in WAL mode, so a file copy can race a writer and lose the
//! tail. Each snapshot is a `<DIR>/brain-<YYYYMMDD-HHMMSS>/` directory holding
//! the two database images plus settings.json, created 0700/0600 in one go.
//! `restore` prepares a sibling home, refuses open SQLite holders using Linux
//! write leases, then atomically exchanges the entire directory. No partial
//! live state exists even when the process dies at the commit boundary.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};

use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use crate::private_fs;

/// Default rotation: how many snapshot directories `prune` keeps.
const DEFAULT_KEEP: usize = 14;

/// `PRAGMA integrity_check` result of a snapshot database, surfaced for the report.
fn integrity(database: &Path) -> Result<String, Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let status: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    Ok(status)
}

/// Copies `source` into `target` through `VACUUM INTO` — a consistent image
/// even when the source is open in WAL mode with an unchecked-pointed tail.
fn vacuum_into(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    // `VACUUM INTO` refuses an existing target: create nothing, let it fail.
    connection.execute("VACUUM INTO ?1", params![target.to_string_lossy().as_ref()])?;
    Ok(())
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        private_fs::create_private_dir_all(parent)?;
    }
    private_fs::write_private(path, bytes)?;
    Ok(())
}

fn utc_stamp() -> String {
    // Compact UTC stamp; filenames sort chronologically.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = days_from_civil_inverse(now.div_euclid(86_400) as i64);
    let time = now.rem_euclid(86_400);
    format!(
        "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}",
        hour = time / 3_600,
        minute = time % 3_600 / 60,
        second = time % 60
    )
}

/// Inverse of days-from-civil for the UTC date of a day count.
fn days_from_civil_inverse(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// The snapshot directory: `--to` when given, else the configured
/// `backup_dir`, else `$BRAIN_HOME/backups`.
fn resolve_backup_dir(brain_home: &Path, to: Option<PathBuf>) -> PathBuf {
    if let Some(to) = to {
        return to;
    }
    let stored = crate::settings::load(brain_home);
    if stored.backup_dir.is_empty() {
        brain_home.join("backups")
    } else {
        PathBuf::from(&stored.backup_dir)
    }
}

fn snapshot_dir_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("brain-{}-{nanos:020}", utc_stamp())
}

fn is_snapshot_dir(name: &str) -> bool {
    // brain-YYYYMMDD-HHMMSS or brain-YYYYMMDD-HHMMSS-<nanos>: the stamped
    // prefix is what matters; the optional disambiguating suffix is digits.
    let Some(stamp) = name.strip_prefix("brain-") else {
        return false;
    };
    let mut parts = stamp.split('-');
    let Some(date) = parts.next() else {
        return false;
    };
    let Some(time) = parts.next() else {
        return false;
    };
    let date_time_ok = date.len() == 8
        && time.len() == 6
        && date
            .bytes()
            .chain(time.bytes())
            .all(|byte| byte.is_ascii_digit());
    let tail_ok = match parts.next() {
        None => true,
        Some(nanos) => !nanos.is_empty() && nanos.bytes().all(|byte| byte.is_ascii_digit()),
    };
    date_time_ok && tail_ok && parts.next().is_none()
}

/// `brain backup create [--to DIR]`.
pub(crate) fn create(brain_home: &Path, to: Option<PathBuf>) -> Result<Value, Box<dyn Error>> {
    private_fs::create_private_dir_all(brain_home)?;
    let lock = std::fs::File::open(brain_home)?;
    lock.try_lock()
        .map_err(|_| "another backup operation is running")?;
    create_unlocked(brain_home, to)
}

fn create_unlocked(brain_home: &Path, to: Option<PathBuf>) -> Result<Value, Box<dyn Error>> {
    let dir = resolve_backup_dir(brain_home, to);
    let published = dir.join(snapshot_dir_name());
    let target = dir.join(format!(
        ".pending-{}",
        published.file_name().unwrap().to_string_lossy()
    ));
    if target.exists() {
        return Err(format!(
            "{} already exists; a snapshot was taken this second — retry",
            target.display()
        )
        .into());
    }
    private_fs::create_private_dir_all(&dir)?;
    // Exclusive creation: an existing directory (including a dangling symlink)
    // never becomes a partially overwritten snapshot.
    std::fs::DirBuilder::new().mode(0o700).create(&target)?;
    let main_db = brain_home.join("brain.sqlite");
    let archive_db = brain_home.join("archive/sessions.sqlite");
    let mut files = Vec::new();
    for (source, name) in [(&main_db, "brain.sqlite"), (&archive_db, "sessions.sqlite")] {
        if !source.is_file() {
            continue;
        }
        let image = target.join(name);
        private_write(&image, &[])?;
        vacuum_into(source, &image)?;
        let integrity = integrity(&image)?;
        if integrity != "ok" {
            return Err(format!("{name}: integrity_check reported {integrity}").into());
        }
        files.push(json!({
            "name": name,
            "path": published.join(name),
            "bytes": std::fs::metadata(&image)?.len(),
            "integrity": integrity,
        }));
    }
    let settings = brain_home.join("settings.json");
    let settings_target = target.join("settings.json");
    if settings.is_file() {
        private_write(&settings_target, &std::fs::read(&settings)?)?;
        files.push(json!({
            "name": "settings.json",
            "path": published.join("settings.json"),
            "bytes": std::fs::metadata(&settings_target)?.len(),
            "integrity": "not a database",
        }));
    }
    // Preserve ordering across directory exchanges and wall-clock rollback.
    let previous = snapshots(&dir).iter().map(|p| snapshot_time(p)).max();
    let created = previous.map_or_else(SystemTime::now, |last| {
        SystemTime::now().max(
            last.checked_add(std::time::Duration::from_nanos(1))
                .unwrap_or(last),
        )
    });
    write_snapshot_time(&target, created)?;
    for entry in std::fs::read_dir(&target)? {
        std::fs::File::open(entry?.path())?.sync_all()?;
    }
    std::fs::File::open(&target)?.sync_all()?;
    std::fs::rename(&target, &published)?;
    std::fs::File::open(&dir)?.sync_all()?;
    Ok(json!({
        "status": "ok",
        "snapshot": published,
        "files": files,
        "note": "databases snapshotted with VACUUM INTO: consistent images, WAL tail included",
    }))
}

/// Snapshot directories inside `dir`, oldest first.
fn snapshots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_snapshot_dir)
        })
        .collect();
    // Filesystem birth time is independent of user-controlled names and does
    // not change when files are added. On filesystems without birth time use
    // directory mtime; ties retain all candidates at the retention boundary.
    names.sort_by_key(|path| (snapshot_time(path), path.clone()));
    names
}

const CREATED_FILE: &str = ".created-ns";

fn write_snapshot_time(path: &Path, time: SystemTime) -> Result<(), Box<dyn Error>> {
    let ns = time.duration_since(UNIX_EPOCH)?.as_nanos();
    private_write(&path.join(CREATED_FILE), ns.to_string().as_bytes())
}

fn snapshot_time(path: &Path) -> SystemTime {
    use std::io::Read;
    // Bounded, no-follow metadata read. Legacy snapshots use filesystem birth
    // time (mtime only when birth time is unavailable). Persist it in staging
    // before a restore recreates the directory hierarchy.
    let stamped = (|| -> Option<SystemTime> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.join(CREATED_FILE))
            .ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        let mut text = String::new();
        file.take(64).read_to_string(&mut text).ok()?;
        let ns: u128 = text.parse().ok()?;
        let seconds = u64::try_from(ns / 1_000_000_000).ok()?;
        UNIX_EPOCH.checked_add(std::time::Duration::new(
            seconds,
            (ns % 1_000_000_000) as u32,
        ))
    })();
    stamped.unwrap_or_else(|| {
        std::fs::symlink_metadata(path)
            .and_then(|m| m.created().or_else(|_| m.modified()))
            .unwrap_or(UNIX_EPOCH)
    })
}

/// A newer empty/corrupt directory cannot evict the last usable image.
fn usable_snapshot(path: &Path) -> bool {
    let mut found = false;
    for name in ["brain.sqlite", "sessions.sqlite", "settings.json"] {
        let file = path.join(name);
        match std::fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => {
                if name != "settings.json" && !integrity(&file).is_ok_and(|s| s == "ok") {
                    return false;
                }
                found = true;
            }
            Ok(_) => return false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    found
}

/// `brain backup list`.
pub(crate) fn list(brain_home: &Path, to: Option<PathBuf>) -> Result<Value, Box<dyn Error>> {
    let dir = resolve_backup_dir(brain_home, to);
    let snapshots = snapshots(&dir);
    let listed: Vec<Value> = snapshots
        .iter()
        .map(|path| {
            let size: u64 = std::fs::read_dir(path)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|entry| entry.metadata().ok())
                        .map(|metadata| metadata.len())
                        .sum()
                })
                .unwrap_or(0);
            json!({
                "path": path,
                "bytes": size,
            })
        })
        .collect();
    Ok(json!({
        "status": "ok",
        "dir": dir,
        "count": listed.len(),
        "snapshots": listed,
    }))
}

/// `brain backup prune --keep N`: keeps the newest N snapshot directories,
/// never removes the last remaining one, and never touches anything that is
/// not a snapshot directory.
pub(crate) fn prune(
    brain_home: &Path,
    to: Option<PathBuf>,
    keep: usize,
) -> Result<Value, Box<dyn Error>> {
    let lock = std::fs::File::open(brain_home)?;
    lock.try_lock()
        .map_err(|_| "another backup operation is running")?;
    let keep = if to.is_none() && keep == DEFAULT_KEEP {
        crate::settings::load(brain_home).backup_keep.max(1) as usize
    } else {
        keep.max(1)
    };
    let dir = resolve_backup_dir(brain_home, to);
    let snapshots = snapshots(&dir);
    let mut removable = snapshots.len().saturating_sub(keep.max(1));
    // Coarse timestamps must never choose an arbitrary "newest" to delete.
    while removable > 0
        && snapshot_time(&snapshots[removable - 1]) == snapshot_time(&snapshots[removable])
        && usable_snapshot(&snapshots[removable - 1])
    {
        removable -= 1;
    }
    let newest_usable = snapshots.iter().rev().find(|path| usable_snapshot(path));
    let mut removed = Vec::new();
    for path in snapshots.iter().take(removable) {
        if Some(path) == newest_usable {
            continue;
        }
        std::fs::remove_dir_all(path)?;
        removed.push(path.clone());
    }
    Ok(json!({
        "status": "ok",
        "keep": keep.max(1),
        "removed": removed,
        "removed_count": removed.len(),
        "remaining": snapshots.len() - removed.len(),
        "note": "only brain-YYYYMMDD-HHMMSS directories are removed; the newest N and any non-snapshot files stay",
    }))
}

/// Refuse symlinks in every existing component before reading or replacing a
/// restore path. `..` is rejected so lexical traversal cannot bypass a check.
fn no_symlinks(path: &Path) -> Result<(), Box<dyn Error>> {
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err("restore paths must not contain .. (pass a snapshot directory listed by `brain backup list`, not a hand-made path)".into());
        }
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => return Err("restore refuses symlinks".into()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Recreate directories privately and hard-link unchanged files. No traversal
/// through symlinks; unrelated symlinks are reproduced as links, never opened.
/// Hard links keep even large local archives/backups on the same filesystem
/// without copying their bytes. Replaced files get new private inodes below.
fn stage_tree(source: &Path, target: &Path) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::DirBuilder::new().mode(0o700).create(&to)?;
            stage_tree(&from, &to)?;
        } else if kind.is_file() {
            std::fs::hard_link(&from, &to)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else {
            return Err("stop Brain services before restore (special file in Brain home): `systemctl --user stop brain-archive-collect.timer brain-backup.timer` and any running `brain ui`/`brain net serve`".into());
        }
    }
    if source
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(is_snapshot_dir)
        && !source.join(CREATED_FILE).try_exists()?
    {
        write_snapshot_time(target, snapshot_time(source))?;
    }
    Ok(())
}

fn sync_tree(path: &Path) -> Result<(), Box<dyn Error>> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        }
    }
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

// Linux-only kernel primitives. The lease is essential: SQLite EXCLUSIVE
// transactions alone do not exclude idle WAL holders. A write lease fails if
// ANY other descriptor/mapping holds the inode, and blocks new opens through
// the directory exchange. Fail closed on filesystems without lease support.
#[cfg(target_os = "linux")]
mod atomic_home {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};

    static BREAK_REQUESTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn break_requested(_: libc::c_int) {
        BREAK_REQUESTED.store(true, Ordering::SeqCst);
    }

    pub(super) struct Leases {
        files: Vec<std::fs::File>,
        previous_handler: libc::sighandler_t,
    }

    impl Leases {
        #[allow(unsafe_code)]
        pub(super) fn acquire(paths: &[PathBuf]) -> Result<Self, Box<dyn Error>> {
            BREAK_REQUESTED.store(false, Ordering::SeqCst);
            // SAFETY: the handler only stores to a lock-free atomic; its
            // address stays valid until process exit. Restore the old handler
            // only after every lease is released.
            let previous_handler = unsafe {
                libc::signal(
                    libc::SIGIO,
                    break_requested as *const () as libc::sighandler_t,
                )
            };
            if previous_handler == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut leases = Self {
                files: Vec::new(),
                previous_handler,
            };
            for path in paths {
                if !path.try_exists()? {
                    continue;
                }
                no_symlinks(path)?;
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(path)?;
                // SAFETY: fcntl takes an owned valid descriptor and an integer
                // lease type, with no pointer arguments.
                if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK) } == -1 {
                    return Err("restore requires no open holders and Linux file-lease support; stop Brain/SQLite clients first".into());
                }
                leases.files.push(file);
            }
            Ok(leases)
        }

        #[allow(unsafe_code)]
        pub(super) fn exchange(&self, home: &Path, staged: &Path) -> Result<(), Box<dyn Error>> {
            use std::os::unix::ffi::OsStrExt;
            let home = std::ffi::CString::new(home.as_os_str().as_bytes())?;
            let staged = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
            if BREAK_REQUESTED.load(Ordering::SeqCst) {
                return Err(
                    "restore interrupted by an attempted open; retry with clients stopped".into(),
                );
            }
            // SAFETY: both C strings outlive the syscall; directory exchange
            // has a single atomic visibility point, including process death.
            let rc = unsafe { rename_exchange(home.as_ptr(), staged.as_ptr()) };
            if rc == -1 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
    }

    /// glibc: the libc wrapper (the process tests inject faults into it).
    /// musl (static release builds) does not export renameat2, so there the
    /// raw syscall is used; the kernel semantics are identical.
    #[cfg(target_env = "gnu")]
    #[allow(unsafe_code)]
    unsafe fn rename_exchange(from: *const libc::c_char, to: *const libc::c_char) -> i64 {
        // SAFETY: the caller passes NUL-terminated paths that outlive the call.
        i64::from(unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                from,
                libc::AT_FDCWD,
                to,
                libc::RENAME_EXCHANGE,
            )
        })
    }

    #[cfg(not(target_env = "gnu"))]
    #[allow(unsafe_code)]
    unsafe fn rename_exchange(from: *const libc::c_char, to: *const libc::c_char) -> i64 {
        // SAFETY: the caller passes NUL-terminated paths that outlive the call.
        unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from,
                libc::AT_FDCWD,
                to,
                libc::RENAME_EXCHANGE,
            )
        }
    }

    impl Drop for Leases {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            for file in &self.files {
                // SAFETY: descriptors remain owned and valid here.
                unsafe {
                    libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_UNLCK);
                }
            }
            // SAFETY: reinstall the handler returned by signal above.
            unsafe {
                libc::signal(libc::SIGIO, self.previous_handler);
            }
        }
    }
}

/// Restore the WHOLE home via one Linux directory exchange. Preparation errors
/// leave the live home unchanged. Process death before/after the exchange
/// leaves respectively the complete old/new state; there is no rename gap or
/// recovery journal that another reader could bypass. The old tree is retained
/// beside the home, and the source snapshot is never consumed.
pub(crate) fn restore(
    brain_home: &Path,
    snapshot: &Path,
    yes: bool,
) -> Result<Value, Box<dyn Error>> {
    no_symlinks(brain_home)?;
    no_symlinks(snapshot)?;
    if !snapshot.is_dir()
        || !snapshot
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(is_snapshot_dir)
    {
        return Err("not a brain-YYYYMMDD-HHMMSS snapshot directory (pick a directory `brain backup list` prints)".into());
    }
    let pairs = [
        ("brain.sqlite", "brain.sqlite"),
        ("sessions.sqlite", "archive/sessions.sqlite"),
        ("settings.json", "settings.json"),
    ];
    let mut plan = Vec::new();
    let mut lease_paths = Vec::new();
    for (name, relative) in pairs {
        let image = snapshot.join(name);
        let target = brain_home.join(relative);
        no_symlinks(&image)?;
        no_symlinks(&target)?;
        // Validate both databases even if only one is being restored: an idle
        // holder of the other DB still refers to the directory being replaced.
        lease_paths.push(target.clone());
        if name != "settings.json" {
            for suffix in ["-wal", "-shm", "-journal"] {
                let side = PathBuf::from(format!("{}{suffix}", target.display()));
                no_symlinks(&side)?;
                lease_paths.push(side);
                no_symlinks(&snapshot.join(format!("{name}{suffix}")))?;
            }
        }
        if !image.try_exists()? {
            continue;
        }
        if !std::fs::symlink_metadata(&image)?.is_file() {
            return Err("snapshot entries must be regular files".into());
        }
        if target.try_exists()? && !std::fs::symlink_metadata(&target)?.is_file() {
            return Err("restore targets must be regular files".into());
        }
        if name != "settings.json" && integrity(&image)? != "ok" {
            return Err("snapshot integrity_check failed".into());
        }
        plan.push(json!({ "name": name, "from": image, "to": target, "relative": relative }));
    }
    if plan.is_empty() {
        return Err("the snapshot holds no restorable files (it is empty or damaged; choose another snapshot from `brain backup list`)".into());
    }
    if !yes {
        return Ok(json!({ "status": "dry-run", "would_restore": plan,
            "note": "stop all Brain/SQLite holders; add --yes for an atomic Linux directory exchange" }));
    }
    #[cfg(not(target_os = "linux"))]
    return Err("atomic restore is supported on Linux only (file leases are a Linux feature; restore this snapshot on the Linux machine that took it)".into());
    #[cfg(target_os = "linux")]
    {
        // Serialize backup operations without creating a lock file. Retain the
        // descriptor across the exchange (it continues to name the old home).
        private_fs::create_private_dir_all(brain_home)?;
        let lock = std::fs::File::open(brain_home)?;
        lock.try_lock()
            .map_err(|_| "another backup operation is running")?;
        let safety = create_unlocked(brain_home, None)?;
        let parent = brain_home.parent().ok_or("Brain home needs a parent")?;
        let staged = parent.join(format!(
            ".brain-restore-{}-{}",
            std::process::id(),
            snapshot_dir_name()
        ));
        // Own the root before installing cleanup; a colliding foreign path
        // is refused without deleting anything from it.
        std::fs::DirBuilder::new().mode(0o700).create(&staged)?;
        let prepared = (|| -> Result<(), Box<dyn Error>> {
            stage_tree(brain_home, &staged)?;
            for entry in &plan {
                let from = Path::new(entry["from"].as_str().ok_or("invalid source")?);
                let relative = entry["relative"].as_str().ok_or("invalid target")?;
                let to = staged.join(relative);
                if let Some(parent) = to.parent() {
                    private_fs::create_private_dir_all(parent)?;
                }
                // Unlink only the staged hard link, never write through it.
                if to.try_exists()? {
                    std::fs::remove_file(&to)?;
                }
                if relative.ends_with(".sqlite") {
                    for suffix in ["-wal", "-shm", "-journal"] {
                        let side = staged.join(format!("{relative}{suffix}"));
                        if side.try_exists()? {
                            std::fs::remove_file(side)?;
                        }
                    }
                    private_write(&to, &[])?;
                    vacuum_into(from, &to)?;
                } else {
                    crate::settings_history::stage_restore(
                        brain_home,
                        &staged,
                        &std::fs::read(from)?,
                    )?;
                }
                std::fs::File::open(&to)?.sync_all()?;
            }
            sync_tree(&staged)?;
            let leases = atomic_home::Leases::acquire(&lease_paths)?;
            leases.exchange(brain_home, &staged)?;
            // There are no fallible steps in the transaction after exchange.
            // Report directory fsync separately: it affects power-loss
            // durability, never process-crash atomicity.
            Ok(())
        })();
        if let Err(error) = prepared {
            // Safe even for symlinks in unrelated parts of the staged tree:
            // remove_dir_all does not follow them. Live home was never moved.
            if staged.is_dir() {
                std::fs::remove_dir_all(&staged)?;
            }
            return Err(error);
        }
        let durable = std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .is_ok();
        Ok(
            json!({ "status": "ok", "restored": plan, "safety_snapshot": safety["snapshot"],
            "previous_home": staged, "parent_synced": durable,
            "note": "all files switched in one atomic directory exchange; original snapshot preserved; previous_home retains displaced files (unrelated files share inodes)" }),
        )
    }
}

/// systemd user units for the daily backup timer (the archive timer's shape).
pub(crate) fn timer_units(binary: &Path) -> Result<(String, String), Box<dyn Error>> {
    let text = binary.to_str().ok_or("binary path is not UTF-8")?;
    if !text.starts_with('/')
        || text
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '\'' | '%' | '$' | ';'))
    {
        return Err("binary path cannot be written safely into a systemd unit; install brain under a plain path (no spaces or quotes, e.g. ~/.brain/bin/brain)".into());
    }
    let service = format!(
        "[Unit]\nDescription=Brain for AI: daily brain snapshot\n\n\
         [Service]\nType=oneshot\nExecStart=\"{}\" backup create\nNice=10\n\
         IOSchedulingClass=idle\nTimeoutStartSec=600\n",
        binary.display()
    );
    let timer = "[Unit]\nDescription=Brain for AI: take a brain snapshot once a day\n\n\
                 [Timer]\nOnCalendar=daily\nPersistent=true\nAccuracySec=10min\n\n\
                 [Install]\nWantedBy=timers.target\n"
        .to_owned();
    Ok((service, timer))
}

pub(crate) const TIMER_UNIT: &str = "brain-backup.timer";
pub(crate) const SERVICE_UNIT: &str = "brain-backup.service";

/// Writes (or removes) the unit files; enabling is left to systemctl.
pub(crate) fn write_timer_units(
    home: &Path,
    binary: &Path,
    install: bool,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let dir = home.join(".config/systemd/user");
    let service_path = dir.join(SERVICE_UNIT);
    let timer_path = dir.join(TIMER_UNIT);
    if install {
        let (service, timer) = timer_units(binary)?;
        std::fs::create_dir_all(&dir)?;
        for (path, text) in [(&service_path, service), (&timer_path, timer)] {
            private_fs::write_private(path, text.as_bytes())?;
        }
    } else {
        for path in [&timer_path, &service_path] {
            match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            }
        }
    }
    Ok(vec![service_path, timer_path])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "brain-backup-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".brain/archive")).unwrap();
        crate::test_support::register(root.clone());
        root
    }

    fn seed_brain(brain_home: &Path) {
        let connection = Connection::open(brain_home.join("brain.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE note(id INTEGER PRIMARY KEY, text TEXT);
             INSERT INTO note VALUES (1, 'synthetic note one');",
            )
            .unwrap();
        connection.close().unwrap();
    }

    #[test]
    fn create_snapshot_is_consistent_with_a_wal_tail_and_private() {
        let root = scratch("create-wal");
        let brain_home = root.join(".brain");
        let holder = Connection::open(brain_home.join("brain.sqlite")).unwrap();
        holder
            .execute_batch(
                "CREATE TABLE note(id INTEGER PRIMARY KEY, text TEXT);
                 INSERT INTO note VALUES (1, 'synthetic note one');",
            )
            .unwrap();
        let _: i64 = holder
            .query_row("PRAGMA wal_autocheckpoint = 0", [], |row| row.get(0))
            .unwrap();
        holder
            .execute(
                "INSERT INTO note VALUES (2, 'synthetic note two, not yet checkpointed')",
                [],
            )
            .unwrap();

        let report = create(&brain_home, None).expect("create");
        assert_eq!(report["status"], "ok");
        let snapshot = PathBuf::from(report["snapshot"].as_str().unwrap());
        let image = snapshot.join("brain.sqlite");
        let mode = std::fs::metadata(&image).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "snapshot file is 0600");
        let dir_mode = std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "snapshot dir is 0700");

        // The WAL tail reached the snapshot even though the holder never
        // checkpointed.
        let image_connection =
            Connection::open_with_flags(&image, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let second: String = image_connection
            .query_row("SELECT text FROM note WHERE id = 2", [], |row| row.get(0))
            .unwrap();
        assert_eq!(second, "synthetic note two, not yet checkpointed");
        drop(holder);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_the_newest_and_never_touches_foreign_files() {
        let root = scratch("prune");
        let brain_home = root.join(".brain");
        seed_brain(&brain_home);
        let dir = brain_home.join("backups");
        std::fs::create_dir_all(dir.join("brain-20260101-000001")).unwrap();
        std::fs::write(dir.join("brain-20260101-000001/brain.sqlite"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("brain-20260102-000002")).unwrap();
        std::fs::write(dir.join("brain-20260102-000002/brain.sqlite"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("brain-20260103-000003")).unwrap();
        std::fs::write(dir.join("brain-20260103-000003/brain.sqlite"), b"x").unwrap();
        for (i, name) in [
            "brain-20260101-000001",
            "brain-20260102-000002",
            "brain-20260103-000003",
        ]
        .iter()
        .enumerate()
        {
            write_snapshot_time(
                &dir.join(name),
                UNIX_EPOCH + std::time::Duration::from_secs(i as u64 + 1),
            )
            .unwrap();
        }
        std::fs::write(dir.join("my-notes.txt"), b"not a snapshot").unwrap();
        std::fs::create_dir_all(dir.join("unrelated-dir")).unwrap();

        let report = prune(&brain_home, None, 2).expect("prune");
        assert_eq!(report["removed_count"], 1, "{report}");
        assert!(!dir.join("brain-20260101-000001").is_dir());
        assert!(dir.join("brain-20260102-000002").is_dir());
        assert!(dir.join("brain-20260103-000003").is_dir());
        assert!(dir.join("my-notes.txt").is_file(), "foreign file untouched");
        assert!(dir.join("unrelated-dir").is_dir(), "foreign dir untouched");

        // keep=1 still keeps the single newest; keep=0 clamps to 1.
        let report = prune(&brain_home, None, 0).expect("prune with zero");
        assert_eq!(report["keep"], 1);
        assert!(
            dir.join("brain-20260103-000003").is_dir(),
            "newest never removed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_dir_and_keep_are_used_by_the_commands() {
        use crate::settings::{self, apply_memory_setting};
        let root = scratch("configured");
        let brain_home = root.join(".brain");
        seed_brain(&brain_home);
        let mut stored = settings::load(&brain_home);
        let custom_dir = format!(
            "/tmp/brain-backup-custom-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        apply_memory_setting(&mut stored, "backup_dir", &custom_dir).unwrap();
        apply_memory_setting(&mut stored, "backup_keep", "2").unwrap();
        settings::save(&brain_home, &stored).unwrap();

        let reloaded = settings::load(&brain_home);
        assert_eq!(
            reloaded.backup_dir, custom_dir,
            "settings::load must see the saved dir"
        );
        let report = create(&brain_home, None).expect("create into the configured dir");
        let snapshot = PathBuf::from(report["snapshot"].as_str().unwrap());
        assert!(
            snapshot.starts_with(&custom_dir),
            "the configured dir is used: {}",
            snapshot.display()
        );
        assert!(snapshot.join("brain.sqlite").is_file());

        let second = PathBuf::from(
            create(&brain_home, None).unwrap()["snapshot"]
                .as_str()
                .unwrap(),
        );
        let third = PathBuf::from(
            create(&brain_home, None).unwrap()["snapshot"]
                .as_str()
                .unwrap(),
        );

        let report = prune(&brain_home, None, DEFAULT_KEEP).expect("prune with settings keep");
        assert_eq!(
            report["keep"], 2,
            "the configured keep wins when --keep is defaulted"
        );
        // Three usable images exist; configured keep=2 drops the oldest.
        assert!(!snapshot.is_dir(), "the oldest snapshot is removed");
        assert!(second.is_dir());
        assert!(third.is_dir());
        // Garbage is refused by the shared parser with the same text as the CLI.
        let error = apply_memory_setting(&mut stored, "backup_keep", "0")
            .expect_err("zero snapshots makes no sense");
        assert!(error.contains("1 to 1000"), "{error}");
        let error = apply_memory_setting(&mut stored, "backup_dir", "relative/path")
            .expect_err("relative path");
        assert!(error.contains("absolute path"), "{error}");
        let _ = std::fs::remove_dir_all("/tmp/brain-backup-custom");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_replaces_data_and_keeps_a_safety_snapshot() {
        let root = scratch("restore");
        let brain_home = root.join(".brain");
        seed_brain(&brain_home);
        crate::settings::save(&brain_home, &crate::settings::StoredSettings::default()).unwrap();
        let report = create(&brain_home, None).expect("snapshot with note one");
        let snapshot = PathBuf::from(report["snapshot"].as_str().unwrap());

        // The live brain moves on: a second note the snapshot does not know.
        let connection = Connection::open(brain_home.join("brain.sqlite")).unwrap();
        connection
            .execute("INSERT INTO note VALUES (2, 'synthetic note two')", [])
            .unwrap();
        drop(connection);

        // Dry run: nothing changes.
        let dry = restore(&brain_home, &snapshot, false).expect("dry run");
        assert_eq!(dry["status"], "dry-run");
        let count: i64 = Connection::open(brain_home.join("brain.sqlite"))
            .unwrap()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "a dry run must not restore");

        let changed = crate::settings::StoredSettings {
            profile: "max".into(),
            ..Default::default()
        };
        crate::settings::save(&brain_home, &changed).unwrap();
        // Apply: the second note is gone again, and a safety snapshot exists.
        let applied = restore(&brain_home, &snapshot, true).expect("restore");
        assert_eq!(applied["status"], "ok");
        assert_eq!(crate::settings::load(&brain_home).profile, "balanced");
        let audit = serde_json::to_value(crate::settings_history::history(&brain_home, 1).unwrap())
            .unwrap();
        assert_eq!(audit[0]["changes"]["profile"]["before"], "max");
        assert_eq!(audit[0]["changes"]["profile"]["after"], "balanced");
        let count: i64 = Connection::open(brain_home.join("brain.sqlite"))
            .unwrap()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "restored to the snapshot state");
        let safety = PathBuf::from(applied["safety_snapshot"].as_str().unwrap());
        assert!(safety.is_dir(), "safety snapshot exists");
        let safety_count: i64 = Connection::open(safety.join("brain.sqlite"))
            .unwrap()
            .query_row("SELECT count(*) FROM note", [], |row| row.get(0))
            .unwrap();
        assert_eq!(safety_count, 2, "the safety snapshot kept the lost note");
        let _ = std::fs::remove_dir_all(&root);
    }
}
