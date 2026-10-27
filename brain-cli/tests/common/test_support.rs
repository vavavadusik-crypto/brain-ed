//! Test-only helper: temp paths that clean themselves up.
//!
//! Every path handed out here is registered in a thread-local registry; the
//! registry's `Drop` removes the trees when the calling test thread ends —
//! normally or through a panic unwind. A full test run therefore leaves no
//! `/tmp` litter even when a test fails halfway through.

use std::cell::RefCell;
use std::path::PathBuf;

struct Cleanups(RefCell<Vec<PathBuf>>);

impl Drop for Cleanups {
    fn drop(&mut self) {
        for path in self.0.borrow().iter() {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(path);
        }
    }
}

thread_local! {
    static CLEANUPS: Cleanups = const { Cleanups(RefCell::new(Vec::new())) };
}

/// Registers an existing-or-not path for removal when the thread ends.
pub(crate) fn register(path: PathBuf) {
    CLEANUPS.with(|cleanups| cleanups.0.borrow_mut().push(path));
}

/// A unique path under `/tmp` (not created).
pub(crate) fn temp_path(tag: &str) -> PathBuf {
    temp_path_under(&std::env::temp_dir(), tag)
}

// Canonicalize only the existing test temp root, before appending the fixture.
// Symlinks created inside a fixture remain visible to product security checks.
pub(crate) fn temp_path_under(root: &std::path::Path, tag: &str) -> PathBuf {
    let root = std::fs::canonicalize(root).expect("canonical test temp root");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let path = root.join(format!("{tag}-{}-{nanos}", std::process::id()));
    register(path.clone());
    path
}
