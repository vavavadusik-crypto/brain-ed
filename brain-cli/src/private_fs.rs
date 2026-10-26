//! Owner-only creation of the directories and files that hold memory.
//!
//! The process umask decides what `create_dir_all` and `fs::write` produce,
//! and a permissive one (0o002, 0o000) left `~/.brain`, the database and the
//! install backups readable by every local account. Everything Brain creates
//! for itself is therefore created 0700/0600 explicitly. Existing paths are
//! treated conservatively: a symlink is never followed into a chmod, data is
//! never moved or rewritten, and only a directory that is Brain's own default
//! (and owned by this user) is tightened after the fact.

use std::fs;
use std::io;
use std::path::Path;

/// Mode for every directory Brain creates.
#[cfg(unix)]
const PRIVATE_DIR: u32 = 0o700;
/// Mode for every control or data file Brain creates.
#[cfg(unix)]
const PRIVATE_FILE: u32 = 0o600;

/// Fixed refusal for an unusable `HOME`; it never quotes the path.
pub(crate) const UNSAFE_HOME: &str = "refusing: HOME and the Brain home must be absolute UTF-8 paths without control characters (set HOME to an absolute path); nothing was changed";

/// Absolute, UTF-8 and free of control characters (DEL and C1 included).
///
/// Pure: no filesystem access, no canonicalization, no symlink following, so
/// an existing alias such as `/home -> /var/home` stays acceptable. An unset
/// `HOME` reaches Brain as `.` and an empty one as ``, which made relative
/// paths resolve against the current directory; a control character cannot
/// be written safely into TOML, JSON commands or desktop entries.
pub(crate) fn is_safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| !text.chars().any(char::is_control))
}

/// Refuses, before any I/O, when one of `paths` is not `is_safe_absolute`.
pub(crate) fn require_safe_paths(paths: &[&Path]) -> Result<(), Box<dyn std::error::Error>> {
    if paths.iter().all(|path| is_safe_absolute(path)) {
        Ok(())
    } else {
        Err(UNSAFE_HOME.into())
    }
}

/// The guard every integration entry point runs first, dry runs included.
pub(crate) fn require_safe_homes(
    home: &Path,
    brain_home: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    require_safe_paths(&[home, brain_home])
}

/// Creates `path` and any missing ancestors owner-only. Existing components
/// are left exactly as they are.
pub(crate) fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Creates one of Brain's own directories (the default home, its backups,
/// its window state) owner-only, and tightens it when it already exists as a
/// real directory owned by this user. A symlink or a directory owned by
/// someone else is used as it is and never chmodded.
pub(crate) fn ensure_private_home(path: &Path) -> io::Result<()> {
    create_private_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_dir()
            && current_uid() == Some(metadata.uid())
            && metadata.mode() & 0o077 != 0
        {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR))?;
        }
    }
    Ok(())
}

/// This process's user id without `unsafe`: the owner of `/proc/self` on
/// Linux, else the owner of `$HOME`. `None` means "unknown", and callers
/// then neither tighten nor trust anything.
#[cfg(unix)]
pub(crate) fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self")
        .ok()
        .or_else(|| fs::metadata(std::env::var_os("HOME")?).ok())
        .map(|metadata| metadata.uid())
}

/// Writes `bytes` to a new owner-only file beside `path`, then renames it into
/// place. The file is 0600 from its first byte, whatever the umask.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temporary = path.with_extension(format!("private-tmp-{}-{nonce}", std::process::id()));
    write_private_via(path, &temporary, bytes)
}

/// `write_private` through an explicit temporary path. A temporary that
/// already exists is an error and is left exactly as found; only a temporary
/// this call created is removed when a later step fails.
fn write_private_via(path: &Path, temporary: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_FILE);
    }
    let mut file = options.open(temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    pub(crate) fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Every entry under `root` with its bytes (regular files only) and mode,
    /// never following a symlink or opening a special file.
    pub(crate) fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>, u32)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let mode = metadata.permissions().mode();
                if metadata.is_dir() {
                    stack.push(path.clone());
                    out.push((path, None, mode));
                } else if metadata.is_file() {
                    out.push((path.clone(), fs::read(&path).ok(), mode));
                } else {
                    out.push((path, None, mode));
                }
            }
        }
        out.sort();
        out
    }

    /// Context pairs Brain derives from an unusable `HOME`: unset (`.`),
    /// empty, relative, non-UTF-8 and control characters. `base` is an
    /// absolute private fixture so the absolute cases stay inside it.
    pub(crate) fn unsafe_home_pairs(base: &Path) -> Vec<(PathBuf, PathBuf)> {
        use std::os::unix::ffi::OsStrExt;
        let non_utf8 = base.join(std::ffi::OsStr::from_bytes(b"h\xffme"));
        let del = base.join("h\u{7f}me");
        let escape = base.join("h\u{1b}[31mme");
        vec![
            (PathBuf::from("."), PathBuf::from("./.brain")),
            (PathBuf::from(""), PathBuf::from(".brain")),
            (
                PathBuf::from("relative-home"),
                PathBuf::from("relative-home/.brain"),
            ),
            (base.join("ok"), PathBuf::from("relative/.brain")),
            (non_utf8.clone(), non_utf8.join(".brain")),
            (del.clone(), del.join(".brain")),
            (escape.clone(), escape.join(".brain")),
            (base.join("ok"), base.join("ok\n/.brain")),
            (base.join("ok\u{85}"), base.join("ok/.brain")),
        ]
    }

    #[test]
    fn the_home_guard_is_pure_and_quotes_no_path() {
        let base = Path::new("/nonexistent-brain-guard-base");
        for (home, brain_home) in unsafe_home_pairs(base) {
            let error = require_safe_homes(&home, &brain_home)
                .unwrap_err()
                .to_string();
            assert_eq!(error, UNSAFE_HOME, "{home:?}");
        }
        for good in ["/home/user", "/var/home/дом ñ space", "/a//b/"] {
            let home = Path::new(good);
            assert!(
                require_safe_homes(home, &home.join(".brain")).is_ok(),
                "{good}"
            );
        }
        assert!(!base.exists(), "the guard performs no I/O");
    }

    fn scratch(_tag: &str) -> PathBuf {
        let dir = crate::test_support::temp_path("brain-private-fs");
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The modes are explicit, so the result is exact under any umask; the
    /// fully permissive umask itself is exercised by `ui_owner_process`.
    pub(crate) fn with_open_umask<T>(body: impl FnOnce() -> T) -> T {
        body()
    }

    #[test]
    fn a_permissive_umask_still_creates_owner_only_paths_and_keeps_data() {
        let root = scratch("umask");
        let home = root.join("home/.brain");
        with_open_umask(|| {
            ensure_private_home(&home).unwrap();
            create_private_dir_all(&home.join("install-backups/nested")).unwrap();
            write_private(&home.join("state.json"), b"{\"a\":1}").unwrap();
            write_private(&home.join("state.json"), b"{\"a\":2}").unwrap();
        });
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("install-backups")), 0o700);
        assert_eq!(mode(&home.join("install-backups/nested")), 0o700);
        assert_eq!(mode(&home.join("state.json")), 0o600);
        assert_eq!(fs::read(home.join("state.json")).unwrap(), b"{\"a\":2}");

        // An existing open default home is tightened; its data stays.
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(home.join("keep.txt"), b"data").unwrap();
        ensure_private_home(&home).unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(fs::read(home.join("keep.txt")).unwrap(), b"data");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn symlinks_and_existing_explicit_parents_are_never_chmodded() {
        let root = scratch("symlink");
        let target = root.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.join(".brain");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        ensure_private_home(&link).unwrap();
        assert_eq!(mode(&target), 0o755, "a symlinked home is not chmodded");

        let explicit = root.join("shared");
        fs::create_dir_all(&explicit).unwrap();
        fs::set_permissions(&explicit, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir_all(&explicit).unwrap();
        assert_eq!(
            mode(&explicit),
            0o755,
            "an existing explicit parent is left alone"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_failed_write_keeps_foreign_temporaries_and_removes_its_own() {
        let root = scratch("tmp-owner");
        let target = root.join("state.json");
        // Someone else's file already sits at the temporary name.
        let foreign = root.join("state.private-tmp-foreign");
        fs::write(&foreign, b"not ours").unwrap();
        assert!(write_private_via(&target, &foreign, b"new").is_err());
        assert_eq!(
            fs::read(&foreign).unwrap(),
            b"not ours",
            "foreign temp kept"
        );
        assert!(!target.exists());

        // Our own temporary is removed when the final rename fails (the
        // destination is a non-empty directory).
        fs::create_dir_all(target.join("occupied")).unwrap();
        let ours = root.join("state.private-tmp-ours");
        assert!(write_private_via(&target, &ours, b"new").is_err());
        assert!(!ours.exists(), "own temp cleaned up");
        assert!(target.join("occupied").is_dir(), "destination untouched");
        fs::remove_dir_all(root).unwrap();
    }
}
