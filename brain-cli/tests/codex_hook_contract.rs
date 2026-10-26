//! Process-level contract of `brain hook` as a Codex hook: the real binary,
//! a synthetic HOME per child, stdin through a real pipe or a Unix stream
//! socket pair, and bounded waits. Nothing here reads a real home, a client
//! config or the network. The socket tests need `AF_UNIX`, which some
//! sandboxes forbid; there they fail at `UnixStream::pair`, not in the hook.
#![cfg(target_os = "linux")]

#[path = "common/test_support.rs"]
mod test_support;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BRAIN: &str = env!("CARGO_BIN_EXE_brain");
const CHILD_BOUND: Duration = Duration::from_secs(15);
const SECRET: &str = "ghp_SYNTHETICSECRETabcdefghijklmnopqrstuv";
const MIB: usize = 1024 * 1024;

/// Plants one unsafe entry at the given state path.
type MakeEntry = fn(&Fixture, &Path);

struct Fixture {
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    /// A private synthetic HOME with an existing private Brain home.
    fn new(label: &str) -> Self {
        let fixture = Self::without_brain_home(label);
        fs::create_dir(fixture.brain()).unwrap();
        fs::set_permissions(fixture.brain(), fs::Permissions::from_mode(0o700)).unwrap();
        fixture
    }

    fn without_brain_home(_label: &str) -> Self {
        let _nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = test_support::temp_path("brain-hook-contract");
        let home = root.join("home");
        fs::create_dir_all(&home).unwrap();
        for dir in [&root, &home] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { root, home }
    }

    fn brain(&self) -> PathBuf {
        self.home.join(".brain")
    }

    fn journal(&self) -> PathBuf {
        self.brain().join("hook-events.jsonl")
    }

    fn dedupe(&self) -> PathBuf {
        self.brain().join("hook-dedupe.json")
    }

    /// An owner-only read-only file standing in for something the hook must
    /// never truncate, chmod or read.
    fn sentinel(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, b"owner sentinel\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        path
    }

    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        let status = Command::new("mkfifo").arg(&path).status().unwrap();
        assert!(status.success(), "mkfifo");
        path
    }

    fn hook(&self, event: &str, input: Input) -> Outcome {
        run_hook(&self.home, event, input)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_sentinel_untouched(path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.mode() & 0o777, 0o400, "sentinel mode changed");
    assert_eq!(
        fs::read(path).unwrap(),
        b"owner sentinel\n",
        "sentinel content changed"
    );
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().mode() & 0o777
}

enum Input {
    /// A finite writer: `write_all` then close, like Codex.
    Bytes(Vec<u8>),
    /// A writer that keeps sending until the pipe closes or `limit` is hit.
    Flood { limit: usize },
    /// The write end stays open and silent until the child exits.
    Idle,
}

struct Outcome {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
    /// Bytes the writer delivered and whether its write failed (EPIPE).
    written: usize,
    write_failed: bool,
}

fn run_hook(home: &Path, event: &str, input: Input) -> Outcome {
    let started = Instant::now();
    let mut child = Command::new(BRAIN)
        .args(["hook", "--event", event, "--client", "codex"])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (sender, writer_result) = mpsc::channel();
    let mut idle = None;
    match input {
        Input::Bytes(bytes) => {
            thread::spawn(move || {
                let result = stdin.write_all(&bytes);
                drop(stdin);
                let _ = sender.send((bytes.len(), result.is_err()));
            });
        }
        Input::Flood { limit } => {
            thread::spawn(move || {
                let chunk = vec![b' '; MIB];
                let mut written = 0;
                let mut failed = false;
                while written < limit {
                    match stdin.write(&chunk) {
                        Ok(count) => written += count,
                        Err(_) => {
                            failed = true;
                            break;
                        }
                    }
                }
                let _ = sender.send((written, failed));
            });
        }
        Input::Idle => idle = Some((stdin, sender)),
    }
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > CHILD_BOUND {
            let _ = child.kill();
            let _ = child.wait();
            panic!("brain hook exceeded {CHILD_BOUND:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let elapsed = started.elapsed();
    if let Some((stdin, sender)) = idle {
        drop(stdin);
        let _ = sender.send((0, false));
    }
    let (written, write_failed) = writer_result
        .recv_timeout(CHILD_BOUND)
        .expect("writer finished after the child exited");
    Outcome {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
        elapsed,
        written,
        write_failed,
    }
}

fn assert_contract(outcome: &Outcome) {
    assert!(outcome.status.success(), "exit {:?}", outcome.status);
    // One JSON line: `{}`, or the fixed SessionStart notice with exactly the two
    // fields Codex's SessionStartHookSpecificOutputWire allows.
    assert!(
        outcome.stdout.ends_with(b"\n")
            && outcome.stdout.iter().filter(|b| **b == b'\n').count() == 1
    );
    let value: serde_json::Value = serde_json::from_slice(&outcome.stdout).expect("stdout is JSON");
    if value != serde_json::json!({}) {
        let output = &value["hookSpecificOutput"];
        assert_eq!(value.as_object().map(|o| o.len()), Some(1), "{value}");
        assert_eq!(output.as_object().map(|o| o.len()), Some(2), "{value}");
        assert_eq!(output["hookEventName"], "SessionStart");
        assert!(
            output["additionalContext"]
                .as_str()
                .unwrap_or("")
                .starts_with("Brain ED")
        );
    }
    assert!(
        outcome.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&outcome.stderr)
    );
}

fn journal_rows(fixture: &Fixture) -> Vec<Value> {
    fs::read_to_string(fixture.journal())
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON event per line"))
        .collect()
}

/// Nothing Brain wrote may hold secrets, tool I/O or this fixture's path.
fn assert_state_clean(fixture: &Fixture) {
    let root = fixture.root.to_str().unwrap();
    for path in [fixture.journal(), fixture.dedupe()] {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.mode() & 0o400 == 0 {
            continue;
        }
        let text = String::from_utf8(fs::read(&path).unwrap()).unwrap();
        for forbidden in [
            SECRET,
            "tool_input",
            "tool_response",
            "transcript",
            "session-",
            root,
        ] {
            assert!(!text.contains(forbidden), "state retained {forbidden:?}");
        }
    }
}

fn session_start(cwd: &str, transcript: &Path) -> Value {
    json!({
        "session_id": "session-synthetic-0001",
        "transcript_path": transcript,
        "cwd": cwd,
        "hook_event_name": "SessionStart",
        "model": "synthetic-model",
        "permission_mode": "default",
        "source": "startup",
        "api_key": SECRET,
    })
}

fn post_tool_use(cwd: &str, padding: usize) -> Value {
    json!({
        "session_id": "session-synthetic-0002",
        "turn_id": "turn-1",
        "transcript_path": null,
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "model": "synthetic-model",
        "permission_mode": "default",
        "tool_name": "shell",
        "tool_kind": "local_shell",
        "tool_use_id": "call-1",
        "tool_input": {"command": format!("echo {SECRET}")},
        "tool_response": {"output": format!("{SECRET}{}", "x".repeat(padding))},
    })
}

/// Waits for `child` within `CHILD_BOUND`, collecting stdout and stderr.
fn finish(mut child: Child, started: Instant) -> (ExitStatus, Vec<u8>, Vec<u8>, Duration) {
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > CHILD_BOUND {
            let _ = child.kill();
            let _ = child.wait();
            panic!("brain hook exceeded {CHILD_BOUND:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let elapsed = started.elapsed();
    (status, out.join().unwrap(), err.join().unwrap(), elapsed)
}

/// File status flags of one of this process's descriptors, read from
/// `/proc/self/fdinfo` (the same value as `F_GETFL`, without `unsafe`).
fn status_flags(fd: &impl AsRawFd) -> u32 {
    let info = fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).unwrap();
    let flags = info
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .expect("flags line");
    u32::from_str_radix(flags.trim(), 8).unwrap()
}

/// `O_NONBLOCK` on Linux.
const O_NONBLOCK: u32 = 0o4000;

/// Runs the hook with one end of a `UnixStream::pair` as stdin, like Node's
/// `spawn` with `stdio: 'pipe'`. Also returns the status flags of a retained
/// duplicate of that read end before and after: they share one open file
/// description with the child's stdin, so a flag change by the hook shows up.
fn run_socket_hook(home: &Path, event: &str, input: Input) -> (Outcome, u32, u32) {
    let (mut writer, reader) = UnixStream::pair().unwrap();
    let retained = reader.try_clone().unwrap();
    let before = status_flags(&retained);
    let started = Instant::now();
    let child = Command::new(BRAIN)
        .args(["hook", "--event", event, "--client", "codex"])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::from(OwnedFd::from(reader)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, writer_result) = mpsc::channel();
    let mut idle = None;
    match input {
        Input::Bytes(bytes) => {
            thread::spawn(move || {
                let result = writer.write_all(&bytes);
                let _ = writer.shutdown(Shutdown::Write);
                drop(writer);
                let _ = sender.send((bytes.len(), result.is_err()));
            });
        }
        Input::Idle => idle = Some((writer, sender)),
        Input::Flood { .. } => unreachable!("not used with sockets"),
    }
    let (status, stdout, stderr, elapsed) = finish(child, started);
    if let Some((writer, sender)) = idle {
        drop(writer);
        let _ = sender.send((0, false));
    }
    let (written, write_failed) = writer_result
        .recv_timeout(CHILD_BOUND)
        .expect("writer finished after the child exited");
    let after = status_flags(&retained);
    let outcome = Outcome {
        status,
        stdout,
        stderr,
        elapsed,
        written,
        write_failed,
    };
    (outcome, before, after)
}

fn assert_flags_unchanged(before: u32, after: u32) {
    assert_eq!(
        before, after,
        "the shared stdin description's flags changed"
    );
    assert_eq!(
        before & O_NONBLOCK,
        0,
        "fixture read end started non-blocking"
    );
}

#[test]
fn unix_socket_stdin_short_event_is_journaled_without_changing_flags() {
    let fixture = Fixture::new("socket-short");
    let payload = post_tool_use("/tmp/brain-hook-socket", 64)
        .to_string()
        .into_bytes();
    let (outcome, before, after) =
        run_socket_hook(&fixture.home, "PostToolUse", Input::Bytes(payload));
    assert_contract(&outcome);
    assert!(!outcome.write_failed);
    assert_flags_unchanged(before, after);
    let rows = journal_rows(&fixture);
    assert_eq!(rows.len(), 1, "a stream socket stdin must be read");
    assert_eq!(
        rows[0]["payload"],
        json!({"cwd": "/tmp/brain-hook-socket", "tool_name": "shell", "tool_kind": "local_shell"})
    );
    assert_state_clean(&fixture);
}

#[test]
fn unix_socket_finite_two_mib_is_drained_without_epipe() {
    let fixture = Fixture::new("socket-big");
    let payload = post_tool_use("/tmp/brain-hook-socket-big", 2 * MIB)
        .to_string()
        .into_bytes();
    assert!(payload.len() > 2 * MIB);
    let (outcome, before, after) =
        run_socket_hook(&fixture.home, "PostToolUse", Input::Bytes(payload));
    assert_contract(&outcome);
    assert!(!outcome.write_failed, "finite socket writer saw EPIPE");
    assert_flags_unchanged(before, after);
    assert!(
        !fixture.journal().exists(),
        "oversized metadata is discarded"
    );
}

#[test]
fn idle_unix_socket_finishes_within_bound_without_changing_flags() {
    let fixture = Fixture::new("socket-idle");
    let (outcome, before, after) = run_socket_hook(&fixture.home, "SessionStart", Input::Idle);
    assert_contract(&outcome);
    assert!(
        outcome.elapsed >= Duration::from_millis(1_500),
        "{:?}",
        outcome.elapsed
    );
    assert!(
        outcome.elapsed < Duration::from_secs(10),
        "{:?}",
        outcome.elapsed
    );
    assert_flags_unchanged(before, after);
    assert!(!fixture.journal().exists());
}

/// The hook with a pipe stdin, the given HOME (`None` removes it) and its own
/// working directory. Only the child's environment and cwd are set.
fn run_hook_in(cwd: &Path, home: Option<&OsStr>, payload: Vec<u8>) -> Outcome {
    let started = Instant::now();
    let mut command = Command::new(BRAIN);
    command
        .args(["hook", "--event", "PostToolUse", "--client", "codex"])
        .env_clear()
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(home) = home {
        command.env("HOME", home);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let written = payload.len();
    let write_failed = stdin.write_all(&payload).is_err();
    drop(stdin);
    let (status, stdout, stderr, elapsed) = finish(child, started);
    Outcome {
        status,
        stdout,
        stderr,
        elapsed,
        written,
        write_failed,
    }
}

#[test]
fn unusable_home_never_writes_activity_relative_to_the_working_directory() {
    let fixture = Fixture::without_brain_home("home-relative");
    let private_brain = |path: &Path| {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        path.to_path_buf()
    };
    // An artificial project with its own private `.brain`, which an unset
    // HOME (`./.brain`) or an empty one (`.brain`) would otherwise reach.
    let project = private_brain(&fixture.root.join("project"));
    let project_brain = private_brain(&project.join(".brain"));
    let relative_brain = private_brain(&project.join("relative-home/.brain"));
    let control_home = fixture.root.join("h\u{7f}me");
    let control_brain = private_brain(&control_home.join(".brain"));
    let payload = || {
        post_tool_use("/tmp/brain-hook-relative", 64)
            .to_string()
            .into_bytes()
    };
    for home in [
        None,
        Some(OsStr::new("")),
        Some(OsStr::new("relative-home")),
        Some(control_home.as_os_str()),
    ] {
        let outcome = run_hook_in(&project, home, payload());
        assert_contract(&outcome);
        assert!(!outcome.write_failed);
        for brain in [&project_brain, &relative_brain, &control_brain] {
            assert_eq!(
                fs::read_dir(brain).unwrap().count(),
                0,
                "{home:?} wrote into {}",
                brain.display()
            );
        }
    }
}

#[test]
fn empty_invalid_and_malformed_input_return_exact_empty_object() {
    let fixture = Fixture::new("bad-input");
    for input in [
        b"".to_vec(),
        b"\xff\xfe{\"cwd\":\"/tmp/brain-hook-project\"}".to_vec(),
        b"{\"cwd\": \"/tmp/brain-hook-project\"".to_vec(),
        b"[\"/tmp/brain-hook-project\"]".to_vec(),
        b"null".to_vec(),
    ] {
        let outcome = fixture.hook("PostToolUse", Input::Bytes(input));
        assert_contract(&outcome);
        assert!(!outcome.write_failed);
    }
    assert!(
        !fixture.journal().exists(),
        "bad input must not be journaled"
    );
}

#[test]
fn session_start_full_schema_keeps_only_allowlisted_metadata() {
    let fixture = Fixture::new("session-start");
    let sentinel = fixture.sentinel("transcript-sentinel");
    let link = fixture.root.join("transcript-link.jsonl");
    symlink(&sentinel, &link).unwrap();
    let fifo = fixture.fifo("transcript.fifo");
    // A FIFO transcript would block forever if the hook opened it.
    for (cwd, transcript) in [("/tmp/brain-hook-a", &fifo), ("/tmp/brain-hook-b", &link)] {
        let payload = session_start(cwd, transcript).to_string().into_bytes();
        let outcome = fixture.hook("SessionStart", Input::Bytes(payload));
        assert_contract(&outcome);
        let value: Value = serde_json::from_slice(&outcome.stdout).unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert!(outcome.elapsed < Duration::from_secs(10));
    }
    assert_sentinel_untouched(&sentinel);
    let rows = journal_rows(&fixture);
    assert_eq!(rows.len(), 2);
    for (row, cwd) in rows.iter().zip(["/tmp/brain-hook-a", "/tmp/brain-hook-b"]) {
        assert_eq!(row["event"], "SessionStart");
        assert_eq!(row["client"], "codex");
        assert_eq!(row["payload"], json!({"cwd": cwd}));
    }
    assert_eq!(mode(&fixture.journal()), 0o600);
    assert_eq!(mode(&fixture.dedupe()), 0o600);
    assert_state_clean(&fixture);
}

#[test]
fn post_tool_use_keeps_tool_identity_and_drops_tool_io() {
    let fixture = Fixture::new("post-tool");
    let payload = post_tool_use("/tmp/brain-hook-tool", 1_024)
        .to_string()
        .into_bytes();
    let outcome = fixture.hook("PostToolUse", Input::Bytes(payload));
    assert_contract(&outcome);
    let rows = journal_rows(&fixture);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["payload"],
        json!({"cwd": "/tmp/brain-hook-tool", "tool_name": "shell", "tool_kind": "local_shell"})
    );
    assert_state_clean(&fixture);
}

#[test]
fn oversized_payloads_are_drained_without_epipe_and_discarded() {
    let fixture = Fixture::new("oversized");
    for padding in [300 * 1024, 2 * MIB] {
        let payload = post_tool_use("/tmp/brain-hook-big", padding)
            .to_string()
            .into_bytes();
        assert!(payload.len() > 256 * 1024);
        let outcome = fixture.hook("PostToolUse", Input::Bytes(payload));
        assert_contract(&outcome);
        assert!(
            !outcome.write_failed,
            "finite writer saw EPIPE at {padding}"
        );
    }
    assert!(
        !fixture.journal().exists(),
        "oversized metadata must be discarded"
    );
}

#[test]
fn flood_beyond_drain_cap_and_idle_pipe_finish_within_bound() {
    let fixture = Fixture::new("flood");
    let outcome = fixture.hook("PostToolUse", Input::Flood { limit: 64 * MIB });
    assert_contract(&outcome);
    assert!(
        outcome.elapsed < Duration::from_secs(10),
        "{:?}",
        outcome.elapsed
    );
    // The hook stopped reading near its 16 MiB drain cap: the writer was cut
    // off (EPIPE is expected past the cap) instead of being read forever.
    assert!(outcome.write_failed);
    assert!(
        outcome.written <= 18 * MIB,
        "drained {} bytes",
        outcome.written
    );

    let outcome = fixture.hook("SessionStart", Input::Idle);
    assert_contract(&outcome);
    assert!(
        outcome.elapsed >= Duration::from_millis(1_500),
        "{:?}",
        outcome.elapsed
    );
    assert!(
        outcome.elapsed < Duration::from_secs(10),
        "{:?}",
        outcome.elapsed
    );
    assert!(!fixture.journal().exists());
}

#[test]
fn unsafe_brain_home_is_never_created_repaired_or_followed() {
    let payload = || session_start("/tmp/brain-hook-home", Path::new("/nonexistent")).to_string();

    let missing = Fixture::without_brain_home("home-missing");
    assert_contract(&missing.hook("SessionStart", Input::Bytes(payload().into_bytes())));
    assert!(!missing.brain().exists(), "hook created the Brain home");

    let permissive = Fixture::without_brain_home("home-permissive");
    fs::create_dir(permissive.brain()).unwrap();
    fs::set_permissions(permissive.brain(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_contract(&permissive.hook("SessionStart", Input::Bytes(payload().into_bytes())));
    assert_eq!(
        mode(&permissive.brain()),
        0o755,
        "permissive home was chmodded"
    );
    assert_eq!(fs::read_dir(permissive.brain()).unwrap().count(), 0);

    let linked = Fixture::without_brain_home("home-symlink");
    let target = linked.root.join("elsewhere");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, linked.brain()).unwrap();
    assert_contract(&linked.hook("SessionStart", Input::Bytes(payload().into_bytes())));
    assert_eq!(
        fs::read_dir(&target).unwrap().count(),
        0,
        "followed home symlink"
    );

    let shared_parent = Fixture::without_brain_home("home-parent");
    fs::create_dir(shared_parent.brain()).unwrap();
    fs::set_permissions(shared_parent.brain(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&shared_parent.home, fs::Permissions::from_mode(0o777)).unwrap();
    assert_contract(&shared_parent.hook("SessionStart", Input::Bytes(payload().into_bytes())));
    assert_eq!(fs::read_dir(shared_parent.brain()).unwrap().count(), 0);
    assert_eq!(mode(&shared_parent.home), 0o777);
}

#[test]
fn unsafe_journal_and_dedupe_entries_are_left_exactly_as_found() {
    let cases: [(&str, MakeEntry); 4] = [
        ("symlink", |fixture, path| {
            symlink(fixture.sentinel("state-sentinel"), path).unwrap()
        }),
        ("hardlink", |fixture, path| {
            let sentinel = fixture.sentinel("state-sentinel");
            fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o600)).unwrap();
            fs::hard_link(&sentinel, path).unwrap();
        }),
        ("fifo", |_, path| {
            assert!(Command::new("mkfifo").arg(path).status().unwrap().success())
        }),
        ("permissive", |_, path| {
            fs::write(path, b"{}\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }),
    ];
    for (label, make) in cases {
        for dedupe in [false, true] {
            let fixture = Fixture::new(&format!("state-{label}-{dedupe}"));
            let path = if dedupe {
                fixture.dedupe()
            } else {
                fixture.journal()
            };
            make(&fixture, &path);
            let before = fs::symlink_metadata(&path).unwrap();
            let payload = session_start("/tmp/brain-hook-state", Path::new("/nonexistent"));
            let outcome = fixture.hook(
                "SessionStart",
                Input::Bytes(payload.to_string().into_bytes()),
            );
            assert_contract(&outcome);
            assert!(outcome.elapsed < Duration::from_secs(10), "{label}");
            let after = fs::symlink_metadata(&path).unwrap();
            assert_eq!(
                (before.ino(), before.mode(), before.len(), before.nlink()),
                (after.ino(), after.mode(), after.len(), after.nlink()),
                "{label} dedupe={dedupe} changed"
            );
            let sentinel = fixture.root.join("state-sentinel");
            if label == "symlink" {
                assert_sentinel_untouched(&sentinel);
            }
            if label == "hardlink" {
                assert_eq!(fs::read(&sentinel).unwrap(), b"owner sentinel\n");
            }
            // A refused journal records nothing; a refused dedupe still lets
            // the journal through but is itself never replaced.
            if dedupe {
                assert_eq!(journal_rows(&fixture).len(), 1, "{label}");
            }
            assert_state_clean(&fixture);
        }
    }
}

#[test]
fn parallel_hooks_append_whole_lines_and_a_held_lock_does_not_hang() {
    let fixture = Fixture::new("parallel");
    let workers = (0..8)
        .map(|index| {
            let home = fixture.home.clone();
            thread::spawn(move || {
                let cwd = format!("/tmp/brain-hook-parallel-{index}");
                let payload = post_tool_use(&cwd, 64).to_string().into_bytes();
                run_hook(&home, "PostToolUse", Input::Bytes(payload))
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        let outcome = worker.join().unwrap();
        assert_contract(&outcome);
        assert!(outcome.elapsed < Duration::from_secs(10));
    }
    let rows = journal_rows(&fixture);
    assert!(!rows.is_empty() && rows.len() <= 8, "{} rows", rows.len());
    let journal = fs::OpenOptions::new()
        .append(true)
        .open(fixture.journal())
        .unwrap();
    journal.lock().unwrap();
    let before = fs::metadata(fixture.journal()).unwrap().len();
    let payload = post_tool_use("/tmp/brain-hook-locked", 64)
        .to_string()
        .into_bytes();
    let outcome = fixture.hook("PostToolUse", Input::Bytes(payload));
    assert_contract(&outcome);
    assert!(
        outcome.elapsed < Duration::from_secs(10),
        "{:?}",
        outcome.elapsed
    );
    assert_eq!(fs::metadata(fixture.journal()).unwrap().len(), before);
    journal.unlock().unwrap();
    assert_state_clean(&fixture);
}
