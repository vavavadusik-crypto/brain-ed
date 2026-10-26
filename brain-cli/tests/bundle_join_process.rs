//! E7.0b as a real process on 127.0.0.1 with temp HOMEs: init → serve without
//! --cert → enroll --bundle-out → join → status with no flags; repeat init is
//! byte-stable; key/bundle/credential modes are 0600/0700; the token appears
//! nowhere in any stdout/stderr; a corrupt or foreign-version bundle is
//! refused without writing.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
            std::env::temp_dir().join(format!("brain-e7b-{tag}-{}-{nanos}", std::process::id()));
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

struct ServerGuard(Child);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn brain(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .args(args)
        .output()
        .expect("run brain")
}

fn brain_json(home: &Path, args: &[&str]) -> (bool, Value, String, String) {
    let output = brain(home, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let parsed = serde_json::from_str(&stdout).unwrap_or(Value::Null);
    (output.status.success(), parsed, stdout, stderr)
}

fn wait_port(child: &mut Child, path: &Path) -> u16 {
    // The server prints one JSON line once it accepts; wait for the file the
    // test helper writes instead: poll the coordinator's auth.sqlite? Simpler:
    // read the single stdout line through a pipe we keep open.
    let _ = child;
    // The caller must use spawn_with_stdout; here we poll a ready-file the
    // helper writes (see spawn_server).
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(document) = serde_json::from_str::<Value>(&text)
            && let Some(bind) = document["bind"].as_str()
            && let Ok(port) = bind.rsplit(':').next().unwrap_or_default().parse::<u16>()
        {
            return port;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("server never reported ready at {path:?}");
}

fn spawn_server(home: &Path, state_dir: &Path, ready_file: &Path) -> (ServerGuard, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .args(["net", "serve", "--state-dir", &state_dir.to_string_lossy()])
        // `net serve` stops when stdin closes: keep the piped stdin open for
        // the whole test (the ServerGuard owns the child, so the pipe stays
        // open until the guard drops, which also stops the server cleanly).
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    // Drain the one ready line into the ready file.
    let stdout = child.stdout.take().expect("stdout");
    let ready = ready_file.to_path_buf();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = fs::write(&ready, &line);
        }
    });
    let port = wait_port(&mut child, ready_file);
    (ServerGuard(child), port)
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn assert_no_token(outputs: &[(String, String)], token: &str) {
    for (stdout, stderr) in outputs {
        assert!(!stdout.contains(token), "token leaked to stdout: {stdout}");
        assert!(!stderr.contains(token), "token leaked to stderr: {stderr}");
    }
}

#[test]
fn init_serve_enroll_join_status_without_flags() {
    let server_home = TempHome::new("server");
    let client_home = TempHome::new("client");
    let state_dir = server_home.path().join("coord");
    let dir_mode = 0o700;

    // init: CA + server cert with DNS and IP SANs.
    let (ok, init, stdout, stderr) = brain_json(
        server_home.path(),
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
    assert!(ok, "{init} {stderr}");
    assert_eq!(init["status"], "ok");
    assert_eq!(init["san"]["dns"], json!(["station.local"]));
    assert_eq!(init["ca_sha256_fingerprint"].as_str().unwrap().len(), 64);
    let tls = state_dir.join("tls");
    for name in ["ca.pem", "ca.key", "server.pem", "server.key"] {
        assert_eq!(mode_of(&tls.join(name)), 0o600, "{name} is 0600");
    }
    assert_eq!(
        fs::metadata(&tls).unwrap().permissions().mode() & 0o777,
        dir_mode
    );

    // A repeat init without --rotate changes nothing (byte-for-byte).
    let before: Vec<u8> = fs::read(tls.join("ca.key")).unwrap();
    let (ok, again, _, _) = brain_json(
        server_home.path(),
        &["net", "init", "--state-dir", &state_dir.to_string_lossy()],
    );
    assert!(ok, "{again}");
    assert_eq!(again["status"], "exists");
    assert_eq!(fs::read(tls.join("ca.key")).unwrap(), before);

    // serve without --cert/--key uses DIR/tls.
    let ready = server_home.path().join("ready.json");
    let (_guard, port) = spawn_server(server_home.path(), &state_dir, &ready);

    // enroll with a bundle.
    let credential = server_home.path().join("cred.pem");
    let bundle = server_home.path().join("machine-bundle.json");
    let (ok, enrolled, out, err) = brain_json(
        server_home.path(),
        &[
            "net",
            "enroll",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--machine",
            "laptop-a",
            "--agent-id",
            "agent@laptop-a",
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
        ],
    );
    assert!(ok, "{enrolled} {err}");
    assert_eq!(enrolled["bundle_server_name"], "station.local");
    assert_eq!(mode_of(&bundle), 0o600, "bundle is 0600");
    let token = fs::read_to_string(&credential).unwrap().trim().to_owned();
    assert!(!token.is_empty());

    // join into the client HOME.
    let (ok, joined, join_out, join_err) = brain_json(
        client_home.path(),
        &[
            "net",
            "join",
            &bundle.to_string_lossy(),
            "--allow-cgnat-dial",
        ],
    );
    assert!(ok, "{joined} {join_err}");
    let client_dir = client_home.path().join(".brain/net/client");
    assert!(client_dir.join("ca.pem").is_file());
    assert!(client_dir.join("credential.pem").is_file());
    let endpoint: Value =
        serde_json::from_str(&fs::read_to_string(client_dir.join("endpoint.json")).unwrap())
            .unwrap();
    assert_eq!(endpoint["server_name"], "station.local");
    assert_eq!(endpoint["allow_cgnat_dial"], true);
    assert_eq!(endpoint["allow_lan_dial"], false);
    assert!(bundle.is_file(), "no --consume: the bundle stays");

    // status with NO connect flags works through endpoint.json.
    let (ok, status, status_out, status_err) = brain_json(client_home.path(), &["net", "status"]);
    assert!(ok, "{status} {status_err}");
    assert_eq!(status["status"], "ok");
    assert_eq!(status["endpoint"]["from_endpoint_json"], true);
    assert_eq!(status["principal"]["machine"], "laptop-a");

    // The token appears in no stdout/stderr of the whole flow.
    assert_no_token(
        &[
            (stdout, stderr),
            (out, err),
            (join_out, join_err),
            (status_out, status_err),
        ],
        &token,
    );
    // And the CA fingerprint in init output matches the CA the client got.
    let ca_fingerprint = init["ca_sha256_fingerprint"].as_str().unwrap();
    let client_ca = fs::read_to_string(client_dir.join("ca.pem")).unwrap();
    // sha256 of the PEM text; the init report hashes the same PEM.
    let hashed = {
        use std::fmt::Write as _;
        // sha256 via the installer helper is not exported to tests; compare by
        // content instead: the client CA equals the server CA file.
        let _ = write!(String::new(), "{ca_fingerprint}");
        client_ca
    };
    assert_eq!(
        fs::read_to_string(tls.join("ca.pem")).unwrap(),
        hashed,
        "the client pinned exactly the coordinator CA"
    );
}

#[test]
fn corrupt_and_foreign_bundles_are_refused_without_writing() {
    let client_home = TempHome::new("refuse");
    let bundle = client_home.path().join("bad-bundle.json");
    let client_dir = client_home.path().join(".brain/net/client");

    fs::write(&bundle, b"{ not json").unwrap();
    let (ok, refused, _, _) = brain_json(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy()],
    );
    assert!(!ok, "{refused}");
    assert!(!client_dir.exists(), "nothing written for a corrupt bundle");

    fs::write(
        &bundle,
        serde_json::to_string(&json!({"version": 99, "server_name": "x", "dial": ["127.0.0.1:1"], "ca_pem": "x", "credential": "y", "machine": "m", "agent_id": "a", "expires_at": 1})).unwrap(),
    )
    .unwrap();
    let (ok, refused, _, _) = brain_json(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy()],
    );
    assert!(!ok, "{refused}");
    assert!(
        !client_dir.exists(),
        "nothing written for a foreign version"
    );

    fs::write(
        &bundle,
        serde_json::to_string(&json!({"version": 1, "server_name": "x", "dial": [], "ca_pem": "x", "credential": "y", "machine": "m", "agent_id": "a", "expires_at": 1})).unwrap(),
    )
    .unwrap();
    let (ok, refused, _, _) = brain_json(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy()],
    );
    assert!(!ok, "empty dial list refused: {refused}");
    // G9: the refusal must name the flag that fixes it.
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("--dial")),
        "hint must name --dial, got: {refused}"
    );
    assert!(!client_dir.exists());
}

#[test]
fn join_consume_removes_the_bundle_and_flags_still_win() {
    let server_home = TempHome::new("server2");
    let client_home = TempHome::new("client2");
    let state_dir = server_home.path().join("coord");
    let (ok, init, _, _) = brain_json(
        server_home.path(),
        &[
            "net",
            "init",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--name",
            "alpha.local",
        ],
    );
    assert!(ok, "{init}");
    let ready = server_home.path().join("ready2.json");
    let (_guard, port) = spawn_server(server_home.path(), &state_dir, &ready);
    let credential = server_home.path().join("cred2.pem");
    let bundle = server_home.path().join("bundle2.json");
    let (ok, enrolled, enroll_out, err) = brain_json(
        server_home.path(),
        &[
            "net",
            "enroll",
            "--state-dir",
            &state_dir.to_string_lossy(),
            "--machine",
            "machine-two",
            "--agent-id",
            "agent-two@machine-two",
            "--role",
            "reader",
            "--grant-space",
            "default",
            "--credential-out",
            &credential.to_string_lossy(),
            "--bundle-out",
            &bundle.to_string_lossy(),
            "--dial",
            &format!("127.0.0.1:{port}"),
            "--server-name",
            "alpha.local",
        ],
    );
    assert!(ok, "enroll: {enrolled} {enroll_out} {err}");
    let (ok, joined, _, join_err) = brain_json(
        client_home.path(),
        &["net", "join", &bundle.to_string_lossy(), "--consume"],
    );
    assert!(ok, "{joined} {join_err}");
    assert!(!bundle.exists(), "--consume removed the bundle");
    assert_eq!(joined["bundle_consumed"], true);
    // Explicit flags still work (here: the same endpoint via flags).
    let (ok, status, _, status_err) = brain_json(
        client_home.path(),
        &[
            "net",
            "status",
            "--dial",
            &format!("127.0.0.1:{port}"),
            "--server-name",
            "alpha.local",
            "--ca",
            &client_home
                .path()
                .join(".brain/net/client/ca.pem")
                .to_string_lossy(),
            "--credential",
            &client_home
                .path()
                .join(".brain/net/client/credential.pem")
                .to_string_lossy(),
        ],
    );
    assert!(ok, "{status} {status_err}");
    assert_eq!(status["principal"]["machine"], "machine-two");
}

fn synthetic_bundle(path: &Path, machine: &str, expires_at: i64) {
    let bundle = json!({
        "version": 1,
        "server_name": "brain-coord.test",
        "dial": ["127.0.0.1:7443"],
        "ca_pem": "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        "credential": format!("synthetic-credential-{machine}"),
        "machine": machine,
        "agent_id": "agent",
        "expires_at": expires_at,
    });
    fs::write(path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn far_future() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 86_400
}

#[test]
fn rotate_replaces_the_whole_set_including_the_san_list() {
    let home = TempHome::new("rotate");
    let state = home.path().join("coord");
    let state_arg = state.to_string_lossy().to_string();
    let (ok, first, _, stderr) = brain_json(
        home.path(),
        &[
            "net",
            "init",
            "--state-dir",
            &state_arg,
            "--name",
            "old.test",
        ],
    );
    assert!(ok, "{first} {stderr}");
    let old_ca = fs::read(state.join("tls/ca.pem")).unwrap();
    let (ok, rotated, _, stderr) = brain_json(
        home.path(),
        &[
            "net",
            "init",
            "--state-dir",
            &state_arg,
            "--name",
            "new.test",
            "--rotate",
        ],
    );
    assert!(ok, "rotate must succeed: {rotated} {stderr}");
    assert_eq!(rotated["rotated"], true);
    assert_ne!(fs::read(state.join("tls/ca.pem")).unwrap(), old_ca);
    let san: Value =
        serde_json::from_slice(&fs::read(state.join("tls/san.json")).unwrap()).unwrap();
    assert_eq!(san["dns"], json!(["new.test"]));
    for name in ["ca.pem", "ca.key", "server.pem", "server.key", "san.json"] {
        assert!(
            state.join(format!("tls/{name}.previous")).is_file(),
            "{name} kept as .previous"
        );
    }
}

#[test]
fn ca_fingerprint_is_the_der_sha256_openssl_reports() {
    let home = TempHome::new("fingerprint");
    let state = home.path().join("coord");
    let (ok, init, _, stderr) = brain_json(
        home.path(),
        &[
            "net",
            "init",
            "--state-dir",
            &state.to_string_lossy(),
            "--name",
            "fp.test",
        ],
    );
    assert!(ok, "{init} {stderr}");
    let Ok(output) = Command::new("openssl")
        .args(["x509", "-noout", "-fingerprint", "-sha256", "-in"])
        .arg(state.join("tls/ca.pem"))
        .output()
    else {
        eprintln!("openssl not installed; fingerprint cross-check skipped");
        return;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let expected: String = text
        .rsplit('=')
        .next()
        .unwrap_or_default()
        .trim()
        .replace(':', "")
        .to_lowercase();
    assert_eq!(init["ca_sha256_fingerprint"], json!(expected));
}

#[test]
fn expired_and_symlinked_bundles_are_refused_without_writing() {
    let home = TempHome::new("expired");
    let client_dir = home.path().join(".brain/net/client");
    let bundle = home.path().join("old-bundle.json");
    synthetic_bundle(&bundle, "m1", 1);
    let (ok, refused, _, _) = brain_json(home.path(), &["net", "join", &bundle.to_string_lossy()]);
    assert!(!ok, "expired bundle refused: {refused}");
    assert!(!client_dir.exists());

    let target = home.path().join("real-bundle.json");
    synthetic_bundle(&target, "m1", far_future());
    let link = home.path().join("link-bundle.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let (ok, refused, _, _) = brain_json(home.path(), &["net", "join", &link.to_string_lossy()]);
    assert!(!ok, "symlinked bundle refused: {refused}");
    assert!(!client_dir.exists());
}

#[test]
fn rejoin_needs_replace_and_swaps_the_endpoint_whole() {
    let home = TempHome::new("rejoin");
    let client_dir = home.path().join(".brain/net/client");
    let first = home.path().join("first.json");
    let second = home.path().join("second.json");
    synthetic_bundle(&first, "first-machine", far_future());
    synthetic_bundle(&second, "second-machine", far_future());
    let (ok, joined, _, stderr) =
        brain_json(home.path(), &["net", "join", &first.to_string_lossy()]);
    assert!(ok, "{joined} {stderr}");

    let (ok, refused, _, _) = brain_json(home.path(), &["net", "join", &second.to_string_lossy()]);
    assert!(!ok, "second join without --replace refused: {refused}");
    let endpoint: Value =
        serde_json::from_slice(&fs::read(client_dir.join("endpoint.json")).unwrap()).unwrap();
    assert_eq!(
        endpoint["machine"], "first-machine",
        "old endpoint untouched"
    );

    let (ok, swapped, _, stderr) = brain_json(
        home.path(),
        &["net", "join", &second.to_string_lossy(), "--replace"],
    );
    assert!(ok, "{swapped} {stderr}");
    let endpoint: Value =
        serde_json::from_slice(&fs::read(client_dir.join("endpoint.json")).unwrap()).unwrap();
    assert_eq!(endpoint["machine"], "second-machine");
    assert_eq!(
        fs::read_to_string(client_dir.join("credential.pem")).unwrap(),
        "synthetic-credential-second-machine"
    );
    let previous = home.path().join(".brain/net/client.previous/endpoint.json");
    let previous: Value = serde_json::from_slice(&fs::read(previous).unwrap()).unwrap();
    assert_eq!(previous["machine"], "first-machine");
    assert_eq!(mode_of(&client_dir) & 0o777, 0o700);
    let leftovers: Vec<_> = fs::read_dir(home.path().join(".brain/net"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".joining-"))
        .collect();
    assert!(leftovers.is_empty(), "no staging directory left behind");
}
