//! Regression tests for graphify forwarding (the review of 2026-10-24 asked
//! for them): `--graph` on the command line wins over GRAPHIFY_GRAPH_JSON, and
//! a child that dies by signal is reported as that signal, never as success.
//! The fake graphify on PATH prints its argv, so the assertions see exactly
//! what brain forwarded.

#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch(_tag: &str, script: &str) -> (PathBuf, PathBuf) {
    let _nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = test_support::temp_path("brain-graphify");
    let bin = root.join("bin");
    let home = root.join("home");
    fs::create_dir_all(&bin).expect("bin dir");
    fs::create_dir_all(&home).expect("home dir");
    fs::write(bin.join("graphify"), script).expect("write fake graphify");
    fs::set_permissions(bin.join("graphify"), fs::Permissions::from_mode(0o755)).expect("chmod");
    (root, bin)
}

fn brain(bin: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env("PATH", bin)
        .env("GRAPHIFY_GRAPH_JSON", "env-graph.json")
        .env_remove("BRAIN_DATABASE")
        .args(args)
        .output()
        .expect("run brain")
}

#[test]
fn an_explicit_graph_flag_suppresses_the_environment_graph() {
    let (root, bin) = scratch("flag", "#!/bin/sh\nprintf 'ARGV %s\\n' \"$@\"\n");
    let home = root.join("home");

    let explicit = brain(
        &bin,
        &home,
        &["--offline", "query", "anything", "--graph", "explicit.json"],
    );
    assert!(
        explicit.status.success(),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    let forwarded = String::from_utf8_lossy(&explicit.stdout);
    assert!(
        forwarded.contains("ARGV --graph\nARGV explicit.json"),
        "{forwarded}"
    );
    assert!(!forwarded.contains("env-graph.json"), "{forwarded}");

    // Without the flag the environment graph is what gets forwarded.
    let from_env = brain(&bin, &home, &["--offline", "query", "anything"]);
    assert!(
        from_env.status.success(),
        "{}",
        String::from_utf8_lossy(&from_env.stderr)
    );
    let forwarded = String::from_utf8_lossy(&from_env.stdout);
    assert!(
        forwarded.contains("ARGV --graph\nARGV env-graph.json"),
        "{forwarded}"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_sigkilled_graphify_is_reported_as_a_signal_and_fails() {
    let (root, bin) = scratch("kill", "#!/bin/sh\nkill -9 $$\n");
    let home = root.join("home");

    let killed = brain(&bin, &home, &["--offline", "query", "anything"]);
    assert!(!killed.status.success(), "a dead child is never a success");
    let stderr = String::from_utf8_lossy(&killed.stderr);
    assert!(stderr.contains("signal: 9 (SIGKILL)"), "{stderr}");
    // Exits as a shell reports a signal death: 128 + SIGKILL.
    assert_eq!(killed.status.code(), Some(137));

    let _ = fs::remove_dir_all(&root);
}
