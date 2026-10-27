//! Darwin whole-home exchange. flock is advisory, unlike Linux file leases.
use std::{
    error::Error,
    fs::{File, OpenOptions},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(super) struct Leases {
    _files: Vec<File>,
}
impl Leases {
    pub(super) fn acquire(paths: &[PathBuf]) -> Result<Self, Box<dyn Error>> {
        eprintln!(
            "macOS restore: advisory flock only; it cannot exclude ordinary SQLite opens. Stop every Brain/SQLite client before restore."
        );
        let mut files = Vec::new();
        for path in paths {
            if !path.try_exists()? {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(brain_core::platform::SECURE_OPEN_FLAGS)
                .open(path)?;
            if !file.metadata()?.is_file() {
                return Err("restore target is not a regular file".into());
            }
            file.try_lock()
                .map_err(|_| "restore target has an advisory lock; stop clients first")?;
            files.push(file);
        }
        Ok(Self { _files: files })
    }
    #[allow(unsafe_code)]
    pub(super) fn exchange(&self, home: &Path, staged: &Path) -> Result<(), Box<dyn Error>> {
        let home = std::ffi::CString::new(home.as_os_str().as_bytes())?;
        let staged = std::ffi::CString::new(staged.as_os_str().as_bytes())?;
        // SAFETY: both terminated paths remain live for this atomic syscall.
        if unsafe { libc::renamex_np(home.as_ptr(), staged.as_ptr(), libc::RENAME_SWAP) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn swaps_complete_directories_and_refuses_conflicting_flock() {
        let dir = crate::test_support::temp_path("darwin-swap");
        std::fs::create_dir_all(dir.join("live")).unwrap();
        std::fs::create_dir(dir.join("staged")).unwrap();
        let live = dir.join("live");
        let staged = dir.join("staged");
        std::fs::write(live.join("db"), b"old").unwrap();
        std::fs::write(staged.join("db"), b"new").unwrap();
        let paths = [live.join("db")];
        let held = Leases::acquire(&paths).unwrap();
        assert!(Leases::acquire(&paths).is_err());
        held.exchange(&live, &staged).unwrap();
        assert_eq!(std::fs::read(live.join("db")).unwrap(), b"new");
        assert_eq!(std::fs::read(staged.join("db")).unwrap(), b"old");
        // fcntl is linked on Darwin and flock is released when descriptors close.
        assert!(held._files[0].as_raw_fd() >= 0);
        drop(held);
        assert!(Leases::acquire(&[staged.join("db")]).is_ok());
    }
    #[test]
    fn failed_swap_preserves_live_tree_and_symlinks_are_refused() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_support::temp_path("darwin-swap-failure");
        std::fs::create_dir_all(dir.join("live")).unwrap();
        std::fs::write(dir.join("live/db"), b"old").unwrap();
        let leases = Leases::acquire(&[]).unwrap();
        assert!(
            leases
                .exchange(&dir.join("live"), &dir.join("missing"))
                .is_err()
        );
        assert_eq!(std::fs::read(dir.join("live/db")).unwrap(), b"old");
        symlink(dir.join("live/db"), dir.join("link")).unwrap();
        assert!(Leases::acquire(&[dir.join("link")]).is_err());
    }
}
