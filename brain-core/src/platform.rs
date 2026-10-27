//! Small OS boundary shared by the CLI and storage/security checks.
use std::io;

/// Effective process owner, independent of HOME and filesystem ownership.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn current_uid() -> io::Result<u32> {
    // SAFETY: geteuid has no arguments or failure mode on supported POSIX hosts.
    Ok(unsafe { libc::geteuid() })
}

/// Secure final-component opens: always use the target libc's flag values.
#[cfg(unix)]
pub const SECURE_OPEN_FLAGS: i32 = libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

/// Resolve an already-open file, retaining rename/symlink race detection.
#[cfg(target_os = "linux")]
pub fn opened_path(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn opened_path(file: &std::fs::File) -> io::Result<std::path::PathBuf> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    let mut buffer = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes to the live buffer.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let length = buffer
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| io::Error::other("F_GETPATH did not terminate the path"))?;
    Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &buffer[..length],
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_and_nofollow_are_process_properties() {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("brain-platform-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("file");
        std::fs::write(&path, b"synthetic").unwrap();
        assert_eq!(
            current_uid().unwrap(),
            std::fs::metadata(&path).unwrap().uid()
        );
        symlink(&path, dir.join("link")).unwrap();
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(SECURE_OPEN_FLAGS)
                .open(dir.join("link"))
                .is_err()
        );
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(
            opened_path(&file).unwrap(),
            std::fs::canonicalize(&path).unwrap()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_constants_and_open_directory() {
        use std::os::unix::fs::OpenOptionsExt;
        assert_eq!(libc::O_NOFOLLOW, 0x100);
        assert_eq!(libc::RENAME_SWAP, 2);
        assert!(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(SECURE_OPEN_FLAGS | libc::O_DIRECTORY)
                .open(std::env::temp_dir())
                .unwrap()
                .metadata()
                .unwrap()
                .is_dir()
        );
    }
}
