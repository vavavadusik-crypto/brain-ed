//! Shared memory between two independent synthetic installations over REAL
//! loopback TLS (`brain net share` + `brain net serve`): explicit host
//! enable + host-local grant + pinned origin, default deny for old,
//! ungranted, cross-namespace and revoked credentials, atomic pull with
//! conflict rollback, retraction only on pull, and ten simultaneous pulling
//! client processes. One machine, separate HOMEs and stores; not a LAN
//! deployment and not ten physical computers.

#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const BRAIN: &str = env!("CARGO_BIN_EXE_brain");

fn temp_root(_tag: &str) -> PathBuf {
    let _nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = test_support::temp_path("brain-share-process");
    fs::create_dir_all(&root).expect("root");
    root
}

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("openssl binary is required");
    assert!(
        output.status.success(),
        "openssl {:?} failed: {}",
        args.first(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Test-only material: two CAs (one per installation), good leaves, a
/// wrong-name leaf and an expired leaf. Fixed argv, no shell.
fn material(root: &Path) -> PathBuf {
    let dir = root.join("pki");
    fs::create_dir_all(&dir).expect("pki");
    for (name, cn) in [
        ("ca-a", "brain-net-test-ca-a"),
        ("ca-b", "brain-net-test-ca-b"),
    ] {
        openssl(
            &dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.crt"),
                "-subj",
                &format!("/CN={cn}"),
                "-days",
                "2",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign",
            ],
        );
    }
    let leaf = |name: &str, ca: &str, san: &str, validity: &[&str]| {
        openssl(
            &dir,
            &[
                "req",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.csr"),
                "-subj",
                "/CN=brain-net-test-leaf",
            ],
        );
        fs::write(
            dir.join(format!("{name}.ext")),
            format!("subjectAltName={san}\nbasicConstraints=CA:FALSE\n"),
        )
        .expect("ext");
        let mut args: Vec<String> = [
            "x509",
            "-req",
            "-in",
            &format!("{name}.csr"),
            "-CA",
            &format!("{ca}.crt"),
            "-CAkey",
            &format!("{ca}.key"),
            "-out",
            &format!("{name}.crt"),
            "-extfile",
            &format!("{name}.ext"),
        ]
        .iter()
        .map(|arg| (*arg).to_owned())
        .collect();
        args.extend(validity.iter().map(|arg| (*arg).to_owned()));
        openssl(&dir, &args.iter().map(String::as_str).collect::<Vec<_>>());
    };
    leaf(
        "server-a",
        "ca-a",
        "IP:127.0.0.1,DNS:localhost",
        &["-days", "2"],
    );
    leaf(
        "server-b",
        "ca-b",
        "IP:127.0.0.1,DNS:localhost",
        &["-days", "2"],
    );
    leaf("wronghost", "ca-a", "DNS:example.invalid", &["-days", "2"]);
    leaf(
        "expired",
        "ca-a",
        "IP:127.0.0.1,DNS:localhost",
        &["-days", "2"],
    );
    // The expired leaf is re-signed by the same CA through `openssl ca` with
    // fixed, ordered past dates: accepted by every OpenSSL 3.x, unlike
    // `x509 -not_before` (3.4+) or a negative `-days` (refused by 3.5).
    std::fs::create_dir_all(dir.join("ca-db")).expect("ca db");
    std::fs::write(dir.join("ca-db/index.txt"), b"").expect("ca index");
    std::fs::write(dir.join("ca-db/serial"), b"1000\n").expect("ca serial");
    std::fs::write(
        dir.join("ca-db/ca.cnf"),
        "[ca]\ndefault_ca = fixture\n[fixture]\ndatabase = ca-db/index.txt\n\
         new_certs_dir = ca-db\nserial = ca-db/serial\ndefault_md = sha256\n\
         policy = anything\nunique_subject = no\ncopy_extensions = none\n\
         [anything]\ncommonName = optional\n",
    )
    .expect("ca config");
    openssl(
        &dir,
        &[
            "ca",
            "-batch",
            "-notext",
            "-config",
            "ca-db/ca.cnf",
            "-cert",
            "ca-a.crt",
            "-keyfile",
            "ca-a.key",
            "-in",
            "expired.csr",
            "-out",
            "expired.crt",
            "-extfile",
            "expired.ext",
            "-startdate",
            "200101000000Z",
            "-enddate",
            "210101000000Z",
        ],
    );
    for key in [
        "ca-a",
        "ca-b",
        "server-a",
        "server-b",
        "wronghost",
        "expired",
    ] {
        fs::set_permissions(
            dir.join(format!("{key}.key")),
            fs::Permissions::from_mode(0o600),
        )
        .expect("key perm");
    }
    dir
}

fn brain(home: &Path) -> Command {
    let mut command = Command::new(BRAIN);
    command
        .env("HOME", home)
        .env("BRAIN_OFFLINE", "true")
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_AGENT")
        .env_remove("BRAIN_MACHINE_ID");
    command
}

/// Every captured output, so the test can prove no secret ever appeared.
#[derive(Default)]
struct Transcript(Vec<u8>);

impl Transcript {
    fn record(&mut self, output: &Output) -> (i32, Value) {
        self.0.extend_from_slice(&output.stdout);
        self.0.extend_from_slice(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "stdout is not JSON (exit {code}): {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (code, value)
    }

    fn run(&mut self, command: &mut Command) -> (i32, Value) {
        let output = command.output().expect("spawn brain");
        self.record(&output)
    }
}

/// A `brain net serve` child; dropping it kills it if the test failed early.
struct Server {
    child: Option<Child>,
    port: u16,
}

impl Server {
    fn start(home: &Path, state: &Path, cert: &Path, key: &Path) -> Self {
        let mut child = brain(home)
            .args(["net", "serve", "--state-dir"])
            .arg(state)
            .arg("--cert")
            .arg(cert)
            .arg("--key")
            .arg(key)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve");
        let mut line = String::new();
        BufReader::new(child.stdout.as_mut().expect("stdout"))
            .read_line(&mut line)
            .expect("ready line");
        let ready: Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("serve did not start: {line:?}"));
        assert_eq!(ready["status"], "serving", "{ready}");
        assert_eq!(ready["lan"], false);
        let bind = ready["bind"].as_str().expect("bind");
        assert!(bind.starts_with("127.0.0.1:"), "loopback only: {bind}");
        let port = bind
            .rsplit(':')
            .next()
            .and_then(|port| port.parse().ok())
            .expect("port");
        Self {
            child: Some(child),
            port,
        }
    }

    fn dial(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Owned stop: close stdin, expect the `stopped` line and exit 0.
    fn stop(mut self) {
        let mut child = self.child.take().expect("child");
        drop(child.stdin.take());
        let mut rest = String::new();
        child
            .stdout
            .take()
            .expect("stdout")
            .read_to_string(&mut rest)
            .expect("rest");
        let status = child.wait().expect("wait serve");
        assert!(status.success(), "serve exit {status:?}");
        assert!(rest.contains("\"stopped\""), "{rest}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Enrolled {
    credential: PathBuf,
    token_id: String,
}

// Test fixture: every argument is one enrollment field of the transcript.
#[allow(clippy::too_many_arguments)]
fn enroll(
    t: &mut Transcript,
    home: &Path,
    state: &Path,
    machine: &str,
    agent: &str,
    role: &str,
    space: &str,
    cred: &Path,
) -> Enrolled {
    let (code, out) = t.run(
        brain(home)
            .args(["net", "enroll", "--state-dir"])
            .arg(state)
            .args([
                "--machine",
                machine,
                "--agent-id",
                agent,
                "--role",
                role,
                "--grant-space",
                space,
                "--credential-out",
            ])
            .arg(cred),
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(out["secret"], "written to credential_file (0600) only");
    let mode = fs::metadata(cred).expect("cred").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    Enrolled {
        credential: cred.to_path_buf(),
        token_id: out["token_id"].as_str().expect("token id").to_owned(),
    }
}

fn share(home: &Path, args: &[&str]) -> Command {
    let mut command = brain(home);
    command.args(["net", "share"]).args(args);
    command
}

fn state_args<'a>(sub: &'a str, state: &'a Path) -> Vec<String> {
    vec![
        sub.to_owned(),
        "--state-dir".to_owned(),
        state.display().to_string(),
    ]
}

fn run_share(
    t: &mut Transcript,
    home: &Path,
    sub: &str,
    state: &Path,
    rest: &[&str],
) -> (i32, Value) {
    let mut args = state_args(sub, state);
    args.extend(rest.iter().map(|arg| (*arg).to_owned()));
    t.run(&mut share(
        home,
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
    ))
}

struct Remote<'a> {
    server: &'a Server,
    ca: PathBuf,
    name: &'static str,
}

fn pull_command(
    home: &Path,
    state: &Path,
    peer: &str,
    origin: &str,
    space: &str,
    remote: &Remote<'_>,
    cred: &Path,
) -> Command {
    let mut command = brain(home);
    command
        .args(["--agent", "admin", "net", "share", "pull", "--state-dir"])
        .arg(state)
        .args([
            "--space",
            space,
            "--peer-id",
            peer,
            "--origin",
            origin,
            "--dial",
            &remote.server.dial(),
            "--server-name",
            remote.name,
            "--ca",
        ])
        .arg(&remote.ca)
        .arg("--credential")
        .arg(cred);
    command
}

fn facts(t: &mut Transcript, home: &Path, state: &Path, retracted: bool) -> Vec<Value> {
    let mut rest = vec!["--space", "team"];
    if retracted {
        rest.push("--include-retracted");
    }
    let (code, out) = run_share(t, home, "facts", state, &rest);
    assert_eq!(code, 0, "{out}");
    out["facts"].as_array().expect("facts").clone()
}

fn http_status(value: &Value) -> i64 {
    value["error"]["http_status"].as_i64().unwrap_or(0)
}

#[test]
fn curated_facts_cross_two_installations_over_real_tls() {
    let root = temp_root("main");
    let pki = material(&root);
    let mut t = Transcript::default();
    let home_a = root.join("install-a");
    let home_b = root.join("install-b");
    for home in [&home_a, &home_b] {
        fs::create_dir_all(home).expect("home");
    }
    let state_a = home_a.join("coord");
    let state_b = home_b.join("coord");
    let creds = root.join("creds");
    fs::create_dir_all(&creds).expect("creds");

    // Host-local administration on A: principals, grants, one curated fact each.
    let b = enroll(
        &mut t,
        &home_a,
        &state_a,
        "m-bbb",
        "agent-b",
        "reader",
        "team",
        &creds.join("b.cred"),
    );
    let old = enroll(
        &mut t,
        &home_a,
        &state_a,
        "m-old",
        "agent-old",
        "worker",
        "team",
        &creds.join("old.cred"),
    );
    let ops = enroll(
        &mut t,
        &home_a,
        &state_a,
        "m-ops",
        "agent-ops",
        "reader",
        "ops",
        &creds.join("ops.cred"),
    );
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "grant",
        &state_a,
        &[
            "--machine",
            "m-bbb",
            "--agent-id",
            "agent-b",
            "--space",
            "team",
        ],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "grant",
        &state_a,
        &[
            "--machine",
            "m-ops",
            "--agent-id",
            "agent-ops",
            "--space",
            "team",
        ],
    );
    assert_eq!(
        (code, http_status(&out)),
        (4, 403),
        "grant outside the namespace is refused: {out}"
    );
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "grant",
        &state_a,
        &[
            "--machine",
            "m-ops",
            "--agent-id",
            "agent-ops",
            "--space",
            "ops",
        ],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "grant",
        &state_a,
        &[
            "--machine",
            "m-old",
            "--agent-id",
            "agent-old",
            "--space",
            "*",
        ],
    );
    assert_eq!(code, 2, "wildcard memory grants do not exist: {out}");
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "publish",
        &state_a,
        &[
            "--space",
            "team",
            "--fact-id",
            "F1",
            "--payload",
            r#"{"project":"brain-for-ai","text":"use WAL"}"#,
        ],
    );
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (3, Some("space_not_enabled")),
        "publish needs host opt-in: {out}"
    );
    let (code, out) = run_share(&mut t, &home_a, "enable", &state_a, &["--space", "team"]);
    assert_eq!(code, 0, "{out}");
    for (id, text) in [("F1", "use WAL"), ("F2", "pin the CA")] {
        let payload = format!(r#"{{"project":"brain-for-ai","text":"{text}"}}"#);
        let (code, out) = run_share(
            &mut t,
            &home_a,
            "publish",
            &state_a,
            &[
                "--space",
                "team",
                "--fact-id",
                id,
                "--payload",
                &payload,
                "--claim",
                text,
                "--claimed-agent",
                "curator-a",
            ],
        );
        assert_eq!(code, 0, "{out}");
    }
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "publish",
        &state_a,
        &[
            "--space",
            "team",
            "--fact-id",
            "BAD",
            "--payload",
            r#"{"project":"x","grant":"memory:read:*"}"#,
        ],
    );
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (3, Some("forbidden_payload_key")),
        "{out}"
    );
    let (code, status_a) = run_share(&mut t, &home_a, "status", &state_a, &[]);
    assert_eq!(code, 0, "{status_a}");
    let origin_a = status_a["origin"].as_str().expect("origin").to_owned();
    assert_eq!(status_a["enabled_spaces"][0]["own_live_facts"], 2);

    // A fork of A's whole state (same origin id) that will later diverge.
    let state_fork = root.join("fork-state");
    fs::create_dir_all(&state_fork).expect("fork");
    for entry in fs::read_dir(&state_a).expect("read a") {
        let entry = entry.expect("entry");
        fs::copy(entry.path(), state_fork.join(entry.file_name())).expect("copy");
    }

    let server_a = Server::start(
        &home_a,
        &state_a,
        &pki.join("server-a.crt"),
        &pki.join("server-a.key"),
    );
    let remote_a = Remote {
        server: &server_a,
        ca: pki.join("ca-a.crt"),
        name: "localhost",
    };

    // B has not opted in to receiving: nothing is applied.
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!(
        (code, out["error"]["code"].as_str()),
        (3, Some("space_not_enabled")),
        "{out}"
    );
    let (code, out) = run_share(&mut t, &home_b, "enable", &state_b, &["--space", "team"]);
    assert_eq!(code, 0, "{out}");

    // Positive transfer with provenance; the global --agent admin is never sent.
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        (out["events"].as_u64(), out["cursor"].as_u64()),
        (Some(2), Some(2)),
        "{out}"
    );
    let got = facts(&mut t, &home_b, &state_b, false);
    assert_eq!(got.len(), 2);
    for fact in &got {
        assert_eq!(fact["origin"], origin_a.as_str());
        assert_eq!(fact["local"], false);
        assert_eq!(fact["trusted_peer"], "peer-a");
        assert_eq!(fact["claimed_agent"], "curator-a");
        assert_eq!(fact["status"], "live");
    }
    assert_eq!(got[0]["payload"]["text"], "use WAL");

    // Idempotent re-pull.
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!(
        (code, out["events"].as_u64(), out["cursor"].as_u64()),
        (0, Some(0), Some(2)),
        "{out}"
    );

    // Wrong pinned origin, wrong CA, wrong name: typed, nothing applied.
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-x",
        "install-0000000000000000",
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!(code, 3, "{out}");
    assert!(
        out["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("origin_mismatch"),
        "{out}"
    );
    let wrong_ca = Remote {
        server: &server_a,
        ca: pki.join("ca-b.crt"),
        name: "localhost",
    };
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &wrong_ca,
        &b.credential,
    ));
    assert_eq!((code, &out["sent"]), (5, &Value::Bool(false)), "{out}");
    let wrong_name = Remote {
        server: &server_a,
        ca: pki.join("ca-a.crt"),
        name: "example.invalid",
    };
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &wrong_name,
        &b.credential,
    ));
    assert_eq!(code, 5, "{out}");

    // Default deny: an old work credential, a cross-namespace credential.
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-old",
        &origin_a,
        "team",
        &remote_a,
        &old.credential,
    ));
    assert_eq!(
        (code, http_status(&out)),
        (4, 403),
        "old credential reads no memory: {out}"
    );
    assert_eq!(out["error"]["detail"]["error"], "forbidden");
    let mut whoami = brain(&home_b);
    whoami
        .args([
            "--agent",
            "admin",
            "net",
            "status",
            "--dial",
            &server_a.dial(),
            "--server-name",
            "localhost",
            "--ca",
        ])
        .arg(pki.join("ca-a.crt"))
        .arg("--credential")
        .arg(&old.credential);
    let (code, out) = t.run(&mut whoami);
    assert_eq!(
        code, 0,
        "the old credential still works for work tools: {out}"
    );
    assert_eq!(
        out["principal"]["agent"], "agent-old",
        "caller-claimed --agent is ignored: {out}"
    );
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-ops",
        &origin_a,
        "team",
        &remote_a,
        &ops.credential,
    ));
    assert_eq!(
        (code, http_status(&out)),
        (4, 403),
        "cross-namespace: {out}"
    );
    let mut query = brain(&home_b);
    query
        .args([
            "--space",
            "ops",
            "net",
            "call",
            "--dial",
            &server_a.dial(),
            "--server-name",
            "localhost",
            "--ca",
        ])
        .arg(pki.join("ca-a.crt"))
        .arg("--credential")
        .arg(&ops.credential)
        .arg("share-query");
    let (code, out) = t.run(&mut query);
    assert_eq!(
        (code, http_status(&out)),
        (4, 403),
        "granted but the host never enabled ops: {out}"
    );

    // Retraction and supersession reach B only when B pulls.
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "retract",
        &state_a,
        &["--space", "team", "--fact-id", "F1"],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "supersede",
        &state_a,
        &[
            "--space",
            "team",
            "--fact-id",
            "F2",
            "--payload",
            r#"{"project":"brain-for-ai","text":"pin the CA, v2"}"#,
        ],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        facts(&mut t, &home_b, &state_b, false).len(),
        2,
        "no push: B still has F1 live before pulling"
    );
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, out["cursor"].as_u64()), (0, Some(4)), "{out}");
    let live = facts(&mut t, &home_b, &state_b, false);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["fact_id"], "F2");
    assert_eq!(live[0]["payload"]["text"], "pin the CA, v2");
    let all = facts(&mut t, &home_b, &state_b, true);
    assert!(
        all.iter()
            .any(|fact| fact["fact_id"] == "F1" && fact["status"] == "retracted"),
        "{all:?}"
    );

    // Host disable stops new reads at once; re-enable resumes.
    let (code, out) = run_share(&mut t, &home_a, "disable", &state_a, &["--space", "team"]);
    assert_eq!((code, &out["changed"]), (0, &Value::Bool(true)), "{out}");
    let (_, st) = run_share(&mut t, &home_a, "status", &state_a, &[]);
    assert_eq!(st["enabled_spaces"], serde_json::json!([]), "{st}");
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, http_status(&out)), (4, 403), "{out}");
    let (code, _) = run_share(&mut t, &home_a, "enable", &state_a, &["--space", "team"]);
    assert_eq!(code, 0);
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "publish",
        &state_a,
        &[
            "--space",
            "team",
            "--fact-id",
            "F3",
            "--payload",
            r#"{"project":"brain-for-ai","text":"after re-enable"}"#,
        ],
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, out["cursor"].as_u64()), (0, Some(5)), "{out}");

    // A diverged fork with the same origin: seq 3 differs -> whole page rolled back.
    let (code, out) = run_share(
        &mut t,
        &home_a,
        "publish",
        &state_fork,
        &[
            "--space",
            "team",
            "--fact-id",
            "EVIL",
            "--payload",
            r#"{"project":"brain-for-ai","text":"forged history"}"#,
        ],
    );
    assert_eq!(code, 0, "{out}");
    let server_fork = Server::start(
        &home_a,
        &state_fork,
        &pki.join("server-b.crt"),
        &pki.join("server-b.key"),
    );
    let remote_fork = Remote {
        server: &server_fork,
        ca: pki.join("ca-b.crt"),
        name: "localhost",
    };
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-fork",
        &origin_a,
        "team",
        &remote_fork,
        &b.credential,
    ));
    assert_eq!(
        (code, out["error"]["code"].as_str(), out["cursor"].as_u64()),
        (3, Some("share_conflict"), Some(0)),
        "{out}"
    );
    assert!(
        facts(&mut t, &home_b, &state_b, true)
            .iter()
            .all(|fact| fact["fact_id"] != "EVIL")
    );
    server_fork.stop();

    // Withdrawing the grant, then revoking the credential, are checked before any replay.
    let (code, _) = run_share(
        &mut t,
        &home_a,
        "ungrant",
        &state_a,
        &[
            "--machine",
            "m-bbb",
            "--agent-id",
            "agent-b",
            "--space",
            "team",
        ],
    );
    assert_eq!(code, 0);
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, http_status(&out)), (4, 403), "{out}");
    let (code, _) = run_share(
        &mut t,
        &home_a,
        "grant",
        &state_a,
        &[
            "--machine",
            "m-bbb",
            "--agent-id",
            "agent-b",
            "--space",
            "team",
        ],
    );
    assert_eq!(code, 0);
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, out["cursor"].as_u64()), (0, Some(5)), "{out}");
    let (code, out) = t.run(
        brain(&home_a)
            .args(["net", "revoke", "--state-dir"])
            .arg(&state_a)
            .args(["--token-id", &b.token_id]),
    );
    assert_eq!(code, 0, "{out}");
    let (code, out) = t.run(&mut pull_command(
        &home_b,
        &state_b,
        "peer-a",
        &origin_a,
        "team",
        &remote_a,
        &b.credential,
    ));
    assert_eq!((code, http_status(&out)), (4, 401), "{out}");

    // Ten simultaneous pulling client processes, each its own installation.
    let mut clients = Vec::new();
    for i in 0..10 {
        let home = root.join(format!("client-{i}"));
        fs::create_dir_all(&home).expect("client home");
        let state = home.join("coord");
        let machine = format!("m-client-{i}");
        let agent = format!("agent-{i}");
        let cred = creds.join(format!("client-{i}.cred"));
        enroll(
            &mut t, &home_a, &state_a, &machine, &agent, "reader", "team", &cred,
        );
        if i != 3 {
            let (code, out) = run_share(
                &mut t,
                &home_a,
                "grant",
                &state_a,
                &[
                    "--machine",
                    &machine,
                    "--agent-id",
                    &agent,
                    "--space",
                    "team",
                ],
            );
            assert_eq!(code, 0, "{out}");
        }
        let (code, _) = run_share(&mut t, &home, "enable", &state, &["--space", "team"]);
        assert_eq!(code, 0);
        clients.push((home, state, cred));
    }
    let started = Instant::now();
    let children: Vec<Child> = clients
        .iter()
        .map(|(home, state, cred)| {
            pull_command(home, state, "peer-a", &origin_a, "team", &remote_a, cred)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn pull")
        })
        .collect();
    let mut ok = 0;
    let mut denied = 0;
    for child in children {
        let output = child.wait_with_output().expect("pull output");
        let (code, out) = t.record(&output);
        match code {
            0 => {
                assert_eq!(out["cursor"].as_u64(), Some(5), "{out}");
                ok += 1;
            }
            4 => {
                assert_eq!(http_status(&out), 403, "{out}");
                denied += 1;
            }
            other => panic!("unexpected exit {other}: {out}"),
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(
        (ok, denied),
        (9, 1),
        "nine granted clients pull, the grant-less one is denied"
    );
    assert!(elapsed < Duration::from_secs(30), "bounded: {elapsed:?}");
    for (home, state, _) in clients.iter().take(2) {
        assert_eq!(facts(&mut t, home, state, false).len(), 2);
    }
    server_a.stop();

    // No private memory store was created or captured in any synthetic HOME,
    // and nothing secret reached any output.
    for home in [&home_a, &home_b] {
        assert!(
            !home.join(".brain").exists(),
            "no private Brain store in {}",
            home.display()
        );
    }
    let mut names: Vec<String> = fs::read_dir(&state_b)
        .expect("b")
        .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        names.iter().all(|name| name.starts_with("share.sqlite")),
        "B holds only its share store: {names:?}"
    );
    let transcript = String::from_utf8_lossy(&t.0).into_owned();
    for cred in fs::read_dir(&creds).expect("creds") {
        let text = fs::read_to_string(cred.expect("c").path()).expect("cred");
        let secret = text.trim().rsplit('.').next().expect("secret");
        assert!(
            secret.len() >= 32 && !transcript.contains(secret),
            "a credential secret leaked"
        );
    }
    let _ = fs::remove_dir_all(&root);
}
