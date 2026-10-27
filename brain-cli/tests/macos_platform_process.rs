//! Native Darwin contracts. Synthetic HOME and fake launchctl; no user agents run.
#![cfg(target_os = "macos")]
#[path = "common/test_support.rs"]
mod test_support;
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = test_support::temp_path("brain-darwin");
        fs::create_dir_all(root.join(".brain")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        for path in [&root, &root.join(".brain")] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        // Model launchd load state and trace argv, never invoke the real service manager.
        let script = root.join("bin/launchctl");
        fs::write(&script,b"#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/launchctl.log\"\ncase \"$1\" in\n print) test -f \"$HOME/loaded\";;\n bootstrap) test ! -f \"$HOME/fail-bootstrap\" || exit 1; touch \"$HOME/loaded\";;\n bootout) test ! -f \"$HOME/fail-bootout\" || exit 1; rm -f \"$HOME/loaded\";;\n *) exit 2;;\nesac\n").unwrap();
        fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_brain"));
        c.env("HOME", &self.0)
            .env("BRAIN_HOME", self.0.join(".brain"))
            .env("BRAIN_MODELS_DIR", self.0.join("models"))
            .env("BRAIN_OFFLINE", "true")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .env_remove("BRAIN_DATABASE")
            .env_remove("BRAIN_SPACE");
        c
    }
    fn cli(&self, args: &[&str]) -> Value {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
#[test]
fn archive_and_backup_launch_agents_bootstrap_bootout_and_report_failure() {
    let f = Fixture::new();
    for (command, label) in [
        ("archive", "org.brain-ed.archive"),
        ("backup", "org.brain-ed.backup"),
    ] {
        let report = f.cli(&[command, "timer", "install"]);
        assert_eq!(report["scheduler"], "launchd");
        assert_eq!(report["status"], "ok");
        let plist = f.0.join(format!("Library/LaunchAgents/{label}.plist"));
        assert!(plist.is_file());
        assert!(
            Command::new("/usr/bin/plutil")
                .args(["-lint", "--"])
                .arg(&plist)
                .status()
                .unwrap()
                .success()
        );
        let text = fs::read_to_string(&plist).unwrap();
        assert!(text.contains("<key>ProgramArguments</key>"));
        assert!(!text.contains("systemd"));
        f.cli(&[command, "timer", "remove"]);
        assert!(!plist.exists());
    }
    fs::write(f.0.join("fail-bootstrap"), b"").unwrap();
    let failed = f.cli(&["archive", "timer", "install"]);
    assert_eq!(failed["status"], "files_written_launchctl_failed");
    assert!(
        f.0.join("Library/LaunchAgents/org.brain-ed.archive.plist")
            .exists()
    );
    assert!(!f.0.join(".config/systemd").exists());
}
#[test]
fn distill_agent_preserves_args_and_status_and_disable_is_idempotent() {
    let f = Fixture::new();
    f.cli(&[
        "llm",
        "add",
        "fixture",
        "--base-url",
        "http://127.0.0.1:9",
        "--model",
        "synthetic",
    ]);
    let report = f.cli(&[
        "distill",
        "schedule",
        "enable",
        "--provider",
        "fixture",
        "--every",
        "30m",
        "--quiet-hours",
        "9-18",
    ]);
    assert_eq!(report["status"], "ok");
    let text =
        fs::read_to_string(f.0.join("Library/LaunchAgents/org.brain-ed.distill.plist")).unwrap();
    for needle in [
        "<integer>1800</integer>",
        "<string>fixture</string>",
        "<string>--quiet-hours</string>",
        "<string>9-18</string>",
        "<string>--database</string>",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    assert_eq!(f.cli(&["distill", "schedule", "status"])["active"], true);
    fs::write(f.0.join("fail-bootout"), b"").unwrap();
    let out = f
        .command()
        .args(["distill", "schedule", "disable"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        f.0.join("Library/LaunchAgents/org.brain-ed.distill.plist")
            .exists()
    );
    fs::remove_file(f.0.join("fail-bootout")).unwrap();
    for _ in 0..2 {
        assert_eq!(f.cli(&["distill", "schedule", "disable"])["status"], "ok");
    }
}
fn bounded_output(mut child: std::process::Child) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Darwin hook exceeded deadline");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

#[allow(unsafe_code)]
fn descriptor_flags(file: &impl std::os::fd::AsRawFd) -> i32 {
    // SAFETY: the object owns a live fd; F_GETFL only reads its integer flags.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    flags
}

#[test]
fn hook_socket_and_regular_file_work_and_blocking_pipe_returns_promptly() {
    let f = Fixture::new();
    let payload=json!({"cwd":"/synthetic/darwin","tool_name":"shell","tool_input":{"secret":"not retained"}}).to_string();
    let input = f.0.join("payload");
    fs::write(&input, &payload).unwrap();
    let out = f
        .command()
        .args(["hook", "--event", "PostToolUse", "--client", "codex"])
        .stdin(fs::File::open(input).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());
    let journal = f.0.join(".brain/hook-events.jsonl");
    assert!(journal.exists());
    assert!(
        !fs::read_to_string(&journal)
            .unwrap()
            .contains("not retained")
    );
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let observer = reader.try_clone().unwrap();
    let flags = descriptor_flags(&observer);
    let child = f
        .command()
        .args(["hook", "--event", "SessionStart", "--client", "codex"])
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(reader)))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .write_all(b"{\"cwd\":\"/synthetic/socket\"}")
        .unwrap();
    writer.shutdown(std::net::Shutdown::Write).unwrap();
    let out = bounded_output(child);
    assert!(out.status.success());
    assert_eq!(descriptor_flags(&observer), flags);
    assert!(
        fs::read_to_string(&journal)
            .unwrap()
            .contains("/synthetic/socket")
    );
    let before = std::time::Instant::now();
    let mut child = f
        .command()
        .args(["hook", "--event", "SessionStart", "--client", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let _held_writer = child.stdin.take().unwrap();
    let out = bounded_output(child);
    assert!(out.status.success());
    assert!(before.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn team_agent_pins_selected_space_and_uses_literal_argv() {
    let f = Fixture::new();
    let project = f.0.join("project & fixtures");
    fs::create_dir(&project).unwrap();
    f.cli(&[
        "team",
        "configure",
        "--machine",
        "worker-1",
        "--project",
        "demo",
        "--root",
        project.to_str().unwrap(),
        "--team-space",
        "team.demo",
        "--recipient",
        "leader:lead",
    ]);
    f.cli(&["team", "enable"]);
    assert_eq!(
        f.cli(&["team", "schedule", "enable", "--every", "15m"])["status"],
        "ok"
    );
    let text = fs::read_to_string(f.0.join("Library/LaunchAgents/org.brain-ed.team-publish.plist"))
        .unwrap();
    for needle in [
        "<integer>900</integer>",
        "<string>--team-space</string>",
        "<string>team.demo</string>",
        "<string>--scheduled</string>",
        "<key>BRAIN_HOME</key>",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    assert_eq!(f.cli(&["team", "schedule", "status"])["enabled"], true);
    assert_eq!(f.cli(&["team", "schedule", "disable"])["status"], "ok");
}
#[test]
fn doctor_distinguishes_darwin_plists_from_loaded_jobs() {
    let f = Fixture::new();
    let before = f.cli(&["doctor"]);
    assert_eq!(before["timers"]["archive"]["enabled_at_boot"], Value::Null);
    assert_eq!(before["timers"]["archive"]["unit_files_installed"], false);
    f.cli(&["archive", "timer", "install"]);
    let after = f.cli(&["doctor"]);
    assert_eq!(after["timers"]["archive"]["unit_files_installed"], true);
    assert_eq!(after["timers"]["archive"]["scheduler"], "launchd");
}
