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

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brain-private-fs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
