//! `brain ui` as a real process on an isolated HOME under a permissive umask:
//! the key never reaches anonymous HTML or stdout, every data route needs it,
//! the owner recovers it from the private owner file, a second launcher on
//! the fixed port reuses the window only when that file matches the actual
//! listening process, and a stale file fails closed.

#![cfg(target_os = "linux")]

#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

fn isolated_home(_tag: &str) -> PathBuf {
    let _nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = test_support::temp_path("brain-ui-owner");
    fs::create_dir_all(&root).expect("create isolated home");
    root
}

/// Runs `brain` under a fully permissive umask (via `sh`, since this crate
/// forbids `unsafe` and so cannot call umask itself).
fn brain(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "umask 000; exec \"$0\" \"$@\"",
            env!("CARGO_BIN_EXE_brain"),
        ])
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env("BRAIN_OFFLINE", "true")
        .args(args)
        .stdin(Stdio::null());
    command
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}

/// One HTTP/1.1 request; returns (status, body).
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 2\r\n"
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n{}");
    stream.write_all(head.as_bytes()).expect("write");
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    let status = answer
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = answer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn wait_for_owner(path: &Path, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.exists() && http(port, "GET", "/api/ping", &[]).0 == 200 {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("brain ui did not start on port {port}");
}

struct Window(Option<Child>);
impl Drop for Window {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn owner_bootstrap_private_state_and_fail_closed_reuse() {
    // Every brain process below runs under umask 000.
    let home = isolated_home("bootstrap");

    // Storage created by an ordinary command is private, data retained.
    let init = brain(&home, &["init"]).output().expect("run init");
    assert!(init.status.success(), "init: {init:?}");
    let brain_home = home.join(".brain");
    assert_eq!(mode(&brain_home), 0o700, "default home");
    let database = brain_home.join("brain.sqlite");
    if database.exists() {
        assert_eq!(mode(&database), 0o600, "database file");
    }

    let port = free_port();
    let port_text = port.to_string();
    let mut window = Window(Some(
        brain(
            &home,
            &[
                "--offline",
                "--space",
                "ui",
                "ui",
                "--port",
                &port_text,
                "--no-open",
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ui"),
    ));
    let owner_path = brain_home.join("ui").join(format!("owner-{port}.json"));
    wait_for_owner(&owner_path, port);
    assert_eq!(mode(&owner_path), 0o600, "owner file");
    assert_eq!(mode(owner_path.parent().unwrap()), 0o700, "ui dir");
    let owner: Value =
        serde_json::from_slice(&fs::read(&owner_path).expect("read owner")).expect("json");
    let token = owner["token"].as_str().expect("token").to_owned();
    assert_eq!(token.len(), 64);
    assert!(
        owner["url"]
            .as_str()
            .unwrap()
            .ends_with(&format!("#k={token}"))
    );

    // Anonymous HTML carries no key; anonymous data routes say 403.
    for path in ["/", "/graph", "/map", "/graph/legacy-3d", "/api/ping"] {
        let (status, body) = http(port, "GET", path, &[]);
        assert_eq!(status, 200, "{path}");
        assert!(!body.contains(&token), "{path} leaks the key");
    }
    for path in [
        "/brain-data.json",
        "/graph/brain-data.json",
        "/api/state",
        "/api/graph",
        "/api/map/projects",
    ] {
        assert_eq!(http(port, "GET", path, &[]).0, 403, "{path} anonymous");
    }

    // The owner, holding the key, sees graph, settings and map.
    let key = ("x-brain-token", token.as_str());
    for path in [
        "/brain-data.json",
        "/api/state",
        "/api/graph",
        "/api/explore/overview",
    ] {
        let (status, body) = http(port, "GET", path, &[key]);
        assert_eq!(status, 200, "{path}: {body}");
    }
    assert_ne!(http(port, "GET", "/api/map/projects", &[key]).0, 403);

    // A foreign origin cannot change anything, key or not.
    let foreign = ("origin", "http://evil.example");
    assert_eq!(http(port, "POST", "/api/settings", &[key, foreign]).0, 403);
    assert_eq!(http(port, "POST", "/api/map/init", &[key, foreign]).0, 403);

    // A second launcher on the fixed port reuses the window it can verify.
    let again = brain(&home, &["ui", "--port", &port_text, "--no-open"])
        .output()
        .expect("second launcher");
    assert!(again.status.success(), "reuse: {again:?}");
    assert!(stdout(&again).contains("already_listening"));
    assert!(!stdout(&again).contains(&token), "reuse printed the key");

    // A stale owner file (another pid) fails closed.
    let mut stale = owner.clone();
    stale["pid"] = Value::from(1);
    let tampered = owner_path.with_extension("tmp");
    fs::write(&tampered, serde_json::to_vec(&stale).unwrap()).unwrap();
    fs::set_permissions(&tampered, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&tampered, &owner_path).unwrap();
    let refused = brain(&home, &["ui", "--port", &port_text, "--no-open"])
        .output()
        .expect("stale launcher");
    assert!(!refused.status.success(), "stale key accepted: {refused:?}");
    assert!(!stdout(&refused).contains(&token));
    assert!(!String::from_utf8_lossy(&refused.stderr).contains(&token));

    // The window's own stdout never carried the key.
    let mut child = window.0.take().expect("ui child");
    let _ = child.kill();
    let output = child.wait_with_output().expect("collect ui");
    assert!(!stdout(&output).contains(&token), "ui printed the key");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&token));
    assert!(stdout(&output).contains("owner_file"));
    fs::remove_dir_all(home).unwrap();
}

/// Reads the fake browser's captured argv once it appears.
fn captured(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the browser was never launched");
}

#[test]
fn the_browser_launch_argv_never_carries_the_key() {
    let home = isolated_home("argv");
    let bin = home.join("fake-bin");
    fs::create_dir_all(&bin).unwrap();
    let capture = home.join("argv.txt");
    let opener = bin.join("xdg-open");
    fs::write(
        &opener,
        format!(
            "#!/bin/sh\nprintf '%s ' \"$0\" \"$@\" >> '{}'\necho >> '{}'\n",
            capture.display(),
            capture.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&opener, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let port = free_port();
    let port_text = port.to_string();
    let mut window = Window(Some(
        brain(&home, &["ui", "--port", &port_text, "--page", "graph"])
            .env("PATH", &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ui"),
    ));
    let brain_home = home.join(".brain");
    let owner_path = brain_home.join("ui").join(format!("owner-{port}.json"));
    wait_for_owner(&owner_path, port);
    let argv = captured(&capture);
    let owner: Value = serde_json::from_slice(&fs::read(&owner_path).unwrap()).unwrap();
    let token = owner["token"].as_str().unwrap().to_owned();
    assert!(!argv.contains(&token), "launch argv leaked the key: {argv}");
    assert!(!argv.contains("#k="), "{argv}");

    // The argv names only the private bootstrap page.
    let bootstrap = brain_home.join("ui").join(format!("open-{port}.html"));
    assert!(argv.contains(bootstrap.to_str().unwrap()), "{argv}");
    assert_eq!(mode(&bootstrap), 0o600, "bootstrap page");
    assert_eq!(mode(bootstrap.parent().unwrap()), 0o700, "ui dir");
    let page = fs::read_to_string(&bootstrap).unwrap();
    let target = page
        .split("url=")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("refresh target");
    assert_eq!(target, format!("http://127.0.0.1:{port}/graph#k={token}"));
    assert!(page.contains("location.replace"));
    let key = target.rsplit_once("#k=").unwrap().1;

    // The key the page forwards is what the data routes need.
    assert_eq!(http(port, "GET", "/brain-data.json", &[]).0, 403);
    assert_eq!(http(port, "GET", "/api/state", &[]).0, 403);
    assert_eq!(
        http(port, "GET", "/brain-data.json", &[("x-brain-token", key)]).0,
        200
    );
    assert_eq!(
        http(port, "GET", "/api/state", &[("x-brain-token", key)]).0,
        200
    );

    // Reuse on the fixed port also launches through the bootstrap page only.
    fs::remove_file(&capture).unwrap();
    let again = brain(&home, &["ui", "--port", &port_text])
        .env("PATH", &path)
        .output()
        .expect("second launcher");
    assert!(again.status.success(), "reuse: {again:?}");
    let argv = captured(&capture);
    assert!(!argv.contains(&token) && argv.contains("open-"), "{argv}");
    assert!(!stdout(&again).contains(&token));
    assert!(!String::from_utf8_lossy(&again.stderr).contains(&token));

    let mut child = window.0.take().expect("ui child");
    let _ = child.kill();
    let output = child.wait_with_output().expect("collect ui");
    assert!(!stdout(&output).contains(&token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&token));
    fs::remove_dir_all(home).unwrap();
}
