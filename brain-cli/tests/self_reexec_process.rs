//! A long-running `brain serve` must start its background worker from its own
//! executable image even after an install renamed a new binary over its path
//! or removed it. Synthetic HOME, disposable binary copy, fake (empty) model
//! cache: no network, no weights, no user state, no user services.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::embeddings::DEFAULT_MODEL2VEC_MODEL;
use serde_json::{Value, json};

/// Serializes this binary's executable-file writes against its spawns.
///
/// The two tests run in parallel threads of ONE process. A file being written
/// (`fs::copy`, `fs::write`) has an `O_WRONLY|O_CLOEXEC` descriptor, and a
/// child forked by the other thread inherits it until that child's own exec.
/// If this test execs its copy inside that window, execve fails with ETXTBSY
/// ("open for writing by one or more processes", execve(2)). `spawn` returns
/// only after the child's exec, so holding this lock over writes and spawns
/// closes the window. It is a fixture race, not the product's re-exec, which
/// runs `/proc/self/exe` of an image nobody writes.
static EXECUTABLE_FIXTURES: Mutex<()> = Mutex::new(());

fn fixture_lock() -> MutexGuard<'static, ()> {
    // A failed test must not poison the other; the guarded data is `()`.
    EXECUTABLE_FIXTURES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

enum PathChange {
    ReplacedAtomically,
    Unlinked,
}

fn scratch(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "brain-self-reexec-{label}-{}-{nonce}",
        std::process::id()
    ));
    let brain_home = root.join("home/.brain");
    fs::create_dir_all(&brain_home).expect("create isolated Brain home");
    // `always` is the mode in which every write starts the worker.
    let settings = json!({"version": 1, "profile": "balanced", "overrides": {},
                          "embedding_mode": "always"});
    fs::write(brain_home.join("settings.json"), settings.to_string()).expect("settings");
    // Empty files satisfy the cache probe only; loading them fails in the worker.
    let repository = brain_home.join(format!(
        "models/models--{}",
        DEFAULT_MODEL2VEC_MODEL.replace('/', "--")
    ));
    fs::create_dir_all(repository.join("snapshots/test")).expect("fake snapshot");
    fs::create_dir_all(repository.join("refs")).expect("fake refs");
    fs::write(repository.join("refs/main"), "test").expect("fake ref");
    for file in ["tokenizer.json", "model.safetensors", "config.json"] {
        fs::write(repository.join("snapshots/test").join(file), b"").expect("fake file");
    }
    fs::create_dir_all(root.join("bin")).expect("bin dir");
    let installed = root.join("bin/brain");
    let _no_spawn_while_writing = fixture_lock();
    fs::copy(env!("CARGO_BIN_EXE_brain"), &installed).expect("copy binary");
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).expect("chmod");
    root
}

fn write_through_old_generation(change: PathChange, label: &str) {
    let root = scratch(label);
    let home = root.join("home");
    let installed = root.join("bin/brain");
    let marker = root.join("new-generation-ran");
    let spawning = fixture_lock();
    let mut server = Command::new(&installed)
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--space",
            "reexec-test",
            "--agent",
            "reexec-agent",
            "--offline",
            "serve",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MCP server from the disposable copy");
    drop(spawning);
    // The server image is loaded once spawn returns; now the install happens.
    match change {
        PathChange::ReplacedAtomically => {
            let staged = root.join("bin/brain.new");
            let _no_spawn_while_writing = fixture_lock();
            fs::write(
                &staged,
                format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            )
            .expect("stage new generation");
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).expect("chmod");
            fs::rename(&staged, &installed).expect("atomic replace");
        }
        PathChange::Unlinked => fs::remove_file(&installed).expect("unlink"),
    }
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "memory_write", "arguments": {"text": "re-exec probe memory", "space": "reexec-test"}}});
    writeln!(server.stdin.as_mut().expect("stdin"), "{request}").expect("send request");
    drop(server.stdin.take());
    let output = server.wait_with_output().expect("finish MCP server");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "serve failed: {stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value =
        serde_json::from_str(stdout.lines().next().expect("response")).expect("JSON-RPC response");
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("tool result text");
    let result: Value = serde_json::from_str(text).expect("tool result JSON");
    assert_eq!(result["status"], "ok", "{result}");
    let warnings = result["warnings"].as_array().expect("warnings");
    assert!(
        warnings
            .iter()
            .all(|w| !w.as_str().unwrap_or("").contains("could not start")),
        "worker must start: {result}"
    );
    // The worker exits on its own (its model files are empty); wait for it to
    // release its lock (the file itself persists) so no process outlives the
    // test, then check which image ran.
    let lock = home.join(".brain/embedding-worker.lock");
    let held = || {
        fs::OpenOptions::new()
            .read(true)
            .open(&lock)
            .is_ok_and(|file| match file.try_lock() {
                Ok(()) => {
                    // Unlock explicitly: a child forked meanwhile shares this
                    // descriptor until it execs and would keep the lock.
                    let _ = file.unlock();
                    false
                }
                Err(error) => matches!(error, fs::TryLockError::WouldBlock),
            })
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    sleep(Duration::from_millis(200));
    while held() && Instant::now() < deadline {
        sleep(Duration::from_millis(50));
    }
    assert!(!held(), "worker did not finish");
    assert!(
        !marker.exists(),
        "the newly installed file must never run as our worker"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn worker_starts_from_the_running_image_after_atomic_replacement() {
    write_through_old_generation(PathChange::ReplacedAtomically, "replaced");
}

#[test]
fn worker_starts_from_the_running_image_after_unlink() {
    write_through_old_generation(PathChange::Unlinked, "unlinked");
}
