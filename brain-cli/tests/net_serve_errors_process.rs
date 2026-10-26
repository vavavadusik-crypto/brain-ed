//! E7.0c process tests: understandable `net serve` failure categories and the
//! `net join` dial-policy check. Temp HOMEs, 127.0.0.1, synthetic material.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct TempHome(PathBuf);

impl TempHome {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("brain-e7c-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn brain(home: &Path, args: &[&str]) -> Output2 {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .args(args)
        .output()
        .expect("run brain");
    Output2::from(output)
}

struct Output2 {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Output2 {
    fn from(output: std::process::Output) -> Self {
        Self {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or(Value::Null)
    }
}

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn init(home: &Path, state_dir: &Path) {
    let output = brain(
        home,
        &[
            "net",
            "init",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--name",
            "station.local",
            "--ip",
            "127.0.0.1",
        ],
    );
    assert!(output.ok, "{}{}", output.stdout, output.stderr);
}

fn spawn_server(home: &Path, state_dir: &Path, ready_file: &Path) -> (ServerGuard, u16) {
    // ServerGuard waits on drop; the never-ready panic path is the only leak.
    #[allow(clippy::zombie_processes)]
    let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .args(["net", "serve", "--state-dir", &state_dir.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    // `net serve` stops when stdin closes: the ServerGuard owns the child, so
    // the piped stdin stays open for the whole test.
    let stdout = child.stdout.take().expect("stdout");
    let ready = ready_file.to_path_buf();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = fs::write(&ready, &line);
        }
    });
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(ready_file)
            && let Ok(document) = serde_json::from_str::<Value>(&text)
            && let Some(bind) = document["bind"].as_str()
            && let Ok(port) = bind.rsplit(':').next().unwrap_or_default().parse::<u16>()
        {
            return (ServerGuard(child), port);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("server never reported ready at {ready_file:?}");
}

#[test]
fn second_serve_on_a_busy_port_reports_address_in_use() {
    let home = TempHome::new("busy");
    let state_dir = home.path().join("coord");
    init(home.path(), &state_dir);
    let ready = home.path().join("ready.json");
    let (_guard, port) = spawn_server(home.path(), &state_dir, &ready);

    // The second serve on the same bound port fails with a clear category.
    let output = brain(
        home.path(),
        &[
            "net",
            "serve",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--port",
            &port.to_string(),
        ],
    );
    assert!(!output.ok, "the second serve must fail: {}", output.stdout);
    let document = output.json();
    assert_eq!(document["category"], "address_in_use", "{document}");
    // The bind probe can race with a checker; either way the reason is
    // understandable and leaks no paths.
    let serialized = document.to_string();
    assert!(
        !serialized.contains(&home.path().to_string_lossy().to_string()),
        "no filesystem paths in the error: {serialized}"
    );
}

#[test]
fn unreadable_material_reports_tls_material() {
    let home = TempHome::new("material");
    let state_dir = home.path().join("coord");
    init(home.path(), &state_dir);
    // Make the key unreadable to this process (no chmod back needed: the dir
    // is owned by the test and removed afterwards).
    let key = state_dir.join("tls/server.key");
    let mut permissions = fs::metadata(&key).unwrap().permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&key, permissions).unwrap();

    let output = brain(
        home.path(),
        &["net", "serve", "--state-dir", &state_dir.to_string_lossy()],
    );
    assert!(!output.ok);
    let document = output.json();
    assert_eq!(document["category"], "tls_material", "{document}");
    // No key contents, no paths.
    let serialized = document.to_string();
    assert!(!serialized.contains("PRIVATE KEY"), "{serialized}");
    assert!(
        !serialized.contains(&state_dir.to_string_lossy().to_string()),
        "no state-dir paths in the error: {serialized}"
    );
}

#[test]
fn join_refuses_a_bundle_whose_dials_are_all_out_of_policy() {
    let server_home = TempHome::new("server3");
    let client_home = TempHome::new("client3");
    let state_dir = server_home.path().join("coord");
    init(server_home.path(), &state_dir);
    let ready = server_home.path().join("ready.json");
    let (_guard, port) = spawn_server(server_home.path(), &state_dir, &ready);
    let credential = server_home.path().join("cred3.pem");
    let bundle = server_home.path().join("bundle3.json");
    let output = brain(
        server_home.path(),
        &[
            "net",
            "enroll",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--machine",
            "machine-three",
            "--agent-id",
            "agent-three@machine-three",
            "--role",
            "worker",
            "--grant-space",
            "default",
            "--credential-out",
            &credential.to_string_lossy(),
            "--bundle-out",
            &bundle.to_string_lossy(),
            "--dial",
            &format!("127.0.0.1:{port}"),
            "--dial",
            "10.0.0.77:7443",
            "--server-name",
            "station.local",
        ],
    );
    assert!(output.ok, "{}{}", output.stdout, output.stderr);

    // No flags: the LAN dial in the bundle is refused, the loopback one is
    // allowed → join succeeds with warnings, endpoint holds the allowed dial.
    let output = brain(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy()],
    );
    assert!(output.ok, "{}{}", output.stdout, output.stderr);
    let joined = output.json();
    assert_eq!(
        joined["refused_dials"][0]["dial"], "10.0.0.77:7443",
        "joined document: {joined}"
    );
    let client_dir = client_home.path().join(".brain/net/client");
    let endpoint: Value =
        serde_json::from_str(&fs::read_to_string(client_dir.join("endpoint.json")).unwrap())
            .unwrap();
    assert_eq!(
        endpoint["dial"],
        json!([format!("127.0.0.1:{port}")]),
        "endpoint: {endpoint}"
    );

    // A bundle whose every dial is out of policy is refused before writing.
    let loopback_bundle = server_home.path().join("lan-bundle.json");
    let output = brain(
        server_home.path(),
        &[
            "net",
            "enroll",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--machine",
            "machine-three",
            "--agent-id",
            "agent-three@machine-three",
            "--role",
            "worker",
            "--grant-space",
            "default",
            "--credential-out",
            &server_home.path().join("cred3b.pem").to_string_lossy(),
            "--bundle-out",
            &loopback_bundle.to_string_lossy(),
            "--dial",
            "10.0.0.77:7443",
            "--server-name",
            "station.local",
        ],
    );
    assert!(output.ok, "{}{}", output.stdout, output.stderr);
    let output = brain(
        client_home.path(),
        &["net", "join", &loopback_bundle.to_string_lossy()],
    );
    assert!(
        !output.ok,
        "a bundle with only out-of-policy dials is refused"
    );
    // Errors from net commands print as JSON on stdout.
    assert!(
        output.stdout.contains("--allow-lan-dial"),
        "the hint names the flag: {}",
        output.stdout
    );
    // The refusal writes nothing: the endpoint from the earlier join is intact.
    let endpoint: Value = serde_json::from_str(
        &fs::read_to_string(client_home.path().join(".brain/net/client/endpoint.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        endpoint["dial"],
        json!([format!("127.0.0.1:{port}")]),
        "the refused join changed nothing"
    );
}

#[test]
fn status_still_works_after_a_join_with_partial_warnings() {
    let server_home = TempHome::new("server4");
    let client_home = TempHome::new("client4");
    let state_dir = server_home.path().join("coord");
    init(server_home.path(), &state_dir);
    let ready = server_home.path().join("ready.json");
    let (_guard, port) = spawn_server(server_home.path(), &state_dir, &ready);
    let credential = server_home.path().join("cred4.pem");
    let bundle = server_home.path().join("bundle4.json");
    let output = brain(
        server_home.path(),
        &[
            "net",
            "enroll",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--machine",
            "machine-four",
            "--agent-id",
            "agent-four@machine-four",
            "--role",
            "worker",
            "--grant-space",
            "default",
            "--credential-out",
            &credential.to_string_lossy(),
            "--bundle-out",
            &bundle.to_string_lossy(),
            "--dial",
            &format!("127.0.0.1:{port}"),
            "--server-name",
            "station.local",
        ],
    );
    assert!(output.ok, "{}{}", output.stdout, output.stderr);
    let joined = brain(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy()],
    );
    assert!(joined.ok, "{}{}", joined.stdout, joined.stderr);
    let status = brain(client_home.path(), &["net", "status"]);
    assert!(status.ok, "{}{}", status.stdout, status.stderr);
    let status_json = status.json();
    assert_eq!(status_json["status"], "ok");
    assert_eq!(status_json["principal"]["machine"], "machine-four");
}
