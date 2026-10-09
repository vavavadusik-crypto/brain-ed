//! `brain net` across real processes over REAL loopback TLS sockets: two
//! coordinator installations (separate homes, state dirs and CAs) and ten
//! simultaneous synthetic client processes, each with its own home and
//! credential. This proves the protocol and its refusals on one machine;
//! it is not a deployment on ten computers and never binds a LAN interface.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const BRAIN: &str = env!("CARGO_BIN_EXE_brain");

fn temp_root(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "brain-net-process-{tag}-{}-{nonce}",
        std::process::id()
    ));
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

fn enroll(
    t: &mut Transcript,
    home: &Path,
    state: &Path,
    machine: &str,
    agent: &str,
    role: &str,
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
                "team",
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

fn call_command(
    home: &Path,
    server: &Server,
    ca: &Path,
    name: &str,
    cred: &Path,
    op: &[&str],
) -> Command {
    let mut command = brain(home);
    command
        .args([
            "--space",
            "team",
            "net",
            "call",
            "--dial",
            &server.dial(),
            "--server-name",
            name,
            "--ca",
        ])
        .arg(ca)
        .arg("--credential")
        .arg(cred)
        .args(op);
    command
}

fn http_status(value: &Value) -> i64 {
    value["error"]["http_status"].as_i64().unwrap_or(0)
}

#[test]
fn two_installations_and_ten_clients_over_real_loopback_tls() {
    let root = temp_root("main");
    let pki = material(&root);
    let pem = |name: &str| pki.join(name);
    let mut t = Transcript::default();

    // Installation A hosts coordinator A; installation B hosts coordinator B.
    let home_a = root.join("install-a");
    let home_b = root.join("install-b");
    let state_a = home_a.join("coord");
    let state_b = home_b.join("coord");
    fs::create_dir_all(&home_a).expect("a");
    fs::create_dir_all(&home_b).expect("b");

    let seed = enroll(
        &mut t,
        &home_a,
        &state_a,
        "m-seed",
        "seed",
        "worker",
        &home_a.join("seed.cred"),
    );
    let reader = enroll(
        &mut t,
        &home_a,
        &state_a,
        "m-reader",
        "bob",
        "reader",
        &home_a.join("reader.cred"),
    );
    let client_homes: Vec<PathBuf> = (0..10)
        .map(|n| root.join(format!("client-{n:02}")))
        .collect();
    let clients: Vec<Enrolled> = client_homes
        .iter()
        .enumerate()
        .map(|(n, home)| {
            fs::create_dir_all(home).expect("client home");
            enroll(
                &mut t,
                &home_a,
                &state_a,
                &format!("m-client-{n:02}"),
                &format!("agent{n:02}"),
                "worker",
                &home.join("cred"),
            )
        })
        .collect();
    let bee = enroll(
        &mut t,
        &home_b,
        &state_b,
        "m-b",
        "bee",
        "worker",
        &home_b.join("bee.cred"),
    );

    let a = Server::start(
        &home_a,
        &state_a,
        &pem("server-a.crt"),
        &pem("server-a.key"),
    );
    let b = Server::start(
        &home_b,
        &state_b,
        &pem("server-b.crt"),
        &pem("server-b.key"),
    );
    let ca_a = pem("ca-a.crt");
    let ca_b = pem("ca-b.crt");

    // Identity comes from the credential: a spoofed global --agent is not sent.
    let (code, who) = t.run(
        call_command(
            &client_homes[0],
            &a,
            &ca_a,
            "localhost",
            &clients[0].credential,
            &["whoami"],
        )
        .args(["--agent", "mallory"]),
    );
    assert_eq!(code, 0, "{who}");
    assert_eq!(who["response"]["principal"]["agent"], "agent00");
    assert_eq!(who["response"]["principal"]["machine"], "m-client-00");
    assert_eq!(who["endpoint"]["system_roots"], false);
    // The IP SAN works as the certificate name too.
    let (code, _) = t.run(&mut call_command(
        &client_homes[1],
        &a,
        &ca_a,
        "127.0.0.1",
        &clients[1].credential,
        &["whoami"],
    ));
    assert_eq!(code, 0);

    // Status reports the actual endpoint, principal and role capabilities.
    let (code, status) = t.run(
        brain(&home_a)
            .args([
                "net",
                "status",
                "--dial",
                &a.dial(),
                "--server-name",
                "localhost",
                "--ca",
            ])
            .arg(&ca_a)
            .arg("--credential")
            .arg(&reader.credential),
    );
    assert_eq!(code, 0, "{status}");
    assert_eq!(status["principal"]["role"], "reader");
    let caps: Vec<&str> = status["capabilities"]
        .as_array()
        .expect("caps")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        caps.contains(&"work_board")
            && !caps.contains(&"work_add")
            && !caps.contains(&"work_claim"),
        "{caps:?}"
    );
    assert_eq!(status["endpoint"]["dial"], a.dial());

    // Seed one task, then ten client processes race for it at once.
    let (code, added) = t.run(&mut call_command(
        &home_a,
        &a,
        &ca_a,
        "localhost",
        &seed.credential,
        &["add", "proj", "contended"],
    ));
    assert_eq!(code, 0, "{added}");
    let task = added["response"]["result"]["task"]["id"]
        .as_i64()
        .expect("task id")
        .to_string();
    let racers: Vec<Child> = client_homes
        .iter()
        .zip(&clients)
        .map(|(home, client)| {
            call_command(
                home,
                &a,
                &ca_a,
                "localhost",
                &client.credential,
                &["claim", &task, "--ttl", "600"],
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn racer")
        })
        .collect();
    let outcomes: Vec<(i32, Value)> = racers
        .into_iter()
        .map(|child| t.record(&child.wait_with_output().expect("racer")))
        .collect();
    let winners: Vec<&Value> = outcomes
        .iter()
        .filter(|(code, _)| *code == 0)
        .map(|(_, value)| value)
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "exactly one TLS client process owns the task: {outcomes:?}"
    );
    for (code, loser) in outcomes.iter().filter(|(code, _)| *code != 0) {
        assert_eq!(*code, 3, "{loser}");
        assert_eq!(loser["response"]["result"]["error_kind"], "busy", "{loser}");
    }
    let winner_machine = winners[0]["response"]["result"]["claim"]["machine"].clone();
    let (code, board) = t.run(&mut call_command(
        &home_a,
        &a,
        &ca_a,
        "localhost",
        &reader.credential,
        &["board", "--project", "proj"],
    ));
    assert_eq!(code, 0, "{board}");
    assert_eq!(
        board["response"]["result"]["tasks"][0]["owner"]["machine"],
        winner_machine
    );

    // A reader may not write.
    let (code, denied) = t.run(&mut call_command(
        &home_a,
        &a,
        &ca_a,
        "localhost",
        &reader.credential,
        &["add", "proj", "nope"],
    ));
    assert_eq!((code, http_status(&denied)), (4, 403), "{denied}");

    // Separate installations are separate auth namespaces.
    let (code, cross) = t.run(&mut call_command(
        &client_homes[0],
        &b,
        &ca_b,
        "localhost",
        &clients[0].credential,
        &["whoami"],
    ));
    assert_eq!(
        (code, http_status(&cross)),
        (4, 401),
        "A's credential at B: {cross}"
    );
    let (code, cross) = t.run(&mut call_command(
        &home_b,
        &a,
        &ca_a,
        "localhost",
        &bee.credential,
        &["whoami"],
    ));
    assert_eq!(
        (code, http_status(&cross)),
        (4, 401),
        "B's credential at A: {cross}"
    );
    let (code, own) = t.run(&mut call_command(
        &home_b,
        &b,
        &ca_b,
        "localhost",
        &bee.credential,
        &["whoami"],
    ));
    assert_eq!(code, 0, "{own}");

    // Revocation is checked before an idempotent replay could be answered.
    let (code, second) = t.run(&mut call_command(
        &home_a,
        &a,
        &ca_a,
        "localhost",
        &seed.credential,
        &["add", "proj", "replayed"],
    ));
    assert_eq!(code, 0, "{second}");
    let second = second["response"]["result"]["task"]["id"]
        .as_i64()
        .expect("id")
        .to_string();
    let claim = ["claim", second.as_str(), "--request-id", "replay-1"];
    let (code, first) = t.run(&mut call_command(
        &client_homes[2],
        &a,
        &ca_a,
        "localhost",
        &clients[2].credential,
        &claim,
    ));
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["request_id"], "replay-1");
    let (code, replay) = t.run(&mut call_command(
        &client_homes[2],
        &a,
        &ca_a,
        "localhost",
        &clients[2].credential,
        &claim,
    ));
    assert_eq!(code, 0, "{replay}");
    assert_eq!(
        replay["response"]["result"]["claim"]["claim_id"],
        first["response"]["result"]["claim"]["claim_id"]
    );
    let (code, revoked) = t.run(
        brain(&home_a)
            .args(["net", "revoke", "--state-dir"])
            .arg(&state_a)
            .args(["--token-id", &clients[2].token_id]),
    );
    assert_eq!(code, 0, "{revoked}");
    let (code, after) = t.run(&mut call_command(
        &client_homes[2],
        &a,
        &ca_a,
        "localhost",
        &clients[2].credential,
        &claim,
    ));
    assert_eq!(
        (code, http_status(&after)),
        (4, 401),
        "no cached replay after revoke: {after}"
    );
    let (code, again) = t.run(
        brain(&home_a)
            .args(["net", "revoke", "--state-dir"])
            .arg(&state_a)
            .args(["--token-id", &clients[2].token_id]),
    );
    assert_eq!((code, again["status"].as_str()), (4, Some("not_found")));
    // Disabling a machine cuts all its credentials.
    let (code, _) = t.run(
        brain(&home_a)
            .args(["net", "revoke", "--state-dir"])
            .arg(&state_a)
            .args(["--machine", "m-client-03"]),
    );
    assert_eq!(code, 0);
    let (code, gone) = t.run(&mut call_command(
        &client_homes[3],
        &a,
        &ca_a,
        "localhost",
        &clients[3].credential,
        &["whoami"],
    ));
    assert_eq!((code, http_status(&gone)), (4, 401), "{gone}");

    // TLS negatives: wrong CA, wrong name; nothing is sent.
    let (code, wrong_ca) = t.run(&mut call_command(
        &client_homes[4],
        &a,
        &ca_b,
        "localhost",
        &clients[4].credential,
        &["whoami"],
    ));
    assert_eq!(
        (code, wrong_ca["sent"].as_bool()),
        (5, Some(false)),
        "{wrong_ca}"
    );
    let (code, wrong_name) = t.run(&mut call_command(
        &client_homes[4],
        &a,
        &ca_a,
        "example.invalid",
        &clients[4].credential,
        &["whoami"],
    ));
    assert_eq!(code, 5, "{wrong_name}");

    // A hostname dial target is refused before any resolution.
    let started = Instant::now();
    let (code, hostname) = t.run(
        brain(&client_homes[4])
            .args([
                "net",
                "call",
                "--dial",
                &format!("localhost:{}", a.port),
                "--server-name",
                "localhost",
                "--ca",
            ])
            .arg(&ca_a)
            .arg("--credential")
            .arg(&clients[4].credential)
            .arg("whoami"),
    );
    assert_eq!(code, 2, "{hostname}");
    assert!(started.elapsed() < Duration::from_secs(3));
    // No token flag exists at all.
    let output = brain(&client_homes[4])
        .args(["net", "call", "--token", "brc1.x.y"])
        .output()
        .expect("spawn");
    assert!(!output.status.success());

    // Plaintext HTTP to the TLS port is never answered in the clear.
    let mut plain = TcpStream::connect(("127.0.0.1", a.port)).expect("connect");
    plain
        .set_read_timeout(Some(Duration::from_secs(8)))
        .expect("timeout");
    let _ = plain
        .write_all(b"POST /v1/call HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}");
    let mut answer = Vec::new();
    let _ = plain.read_to_end(&mut answer);
    assert!(
        !String::from_utf8_lossy(&answer).contains("HTTP/"),
        "plaintext must not be served"
    );

    // Credential by inherited descriptor: a 0600 file works...
    let fd_call = |cred_redirect: &str, cred: &Path| {
        let mut command = Command::new("sh");
        command
            .env("HOME", &client_homes[5])
            .env("BRAIN_OFFLINE", "true")
            .env("CRED", cred)
            .arg("-c")
            .arg(format!("exec \"$0\" \"$@\" {cred_redirect}"))
            .arg(BRAIN)
            .args([
                "net",
                "call",
                "--dial",
                &a.dial(),
                "--server-name",
                "localhost",
                "--ca",
            ])
            .arg(&ca_a)
            .args(["--credential-fd", "3", "whoami"]);
        command
    };
    let (code, by_fd) = t.run(&mut fd_call("3<\"$CRED\"", &clients[5].credential));
    assert_eq!(code, 0, "{by_fd}");
    assert_eq!(by_fd["response"]["principal"]["agent"], "agent05");
    // ...a group/world-readable file, a pipe, and a std descriptor are refused.
    let loose = client_homes[5].join("loose.cred");
    fs::copy(&clients[5].credential, &loose).expect("copy");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("perm");
    let (code, _) = t.run(&mut fd_call("3<\"$CRED\"", &loose));
    assert_eq!(code, 2, "0644 descriptor must be refused");
    let started = Instant::now();
    let (code, _) = t.run(fd_call("3<&0", &loose).stdin(Stdio::piped()));
    assert_eq!(code, 2, "a pipe descriptor must be refused");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a pipe must not block"
    );
    let (code, _) = t.run(
        brain(&client_homes[5])
            .args([
                "net",
                "call",
                "--dial",
                &a.dial(),
                "--server-name",
                "localhost",
                "--ca",
            ])
            .arg(&ca_a)
            .args(["--credential-fd", "2", "whoami"]),
    );
    assert_eq!(code, 2, "std descriptors are never credentials");

    // Principals list carries no secrets.
    let (code, listed) = t.run(
        brain(&home_a)
            .args(["net", "principals", "--state-dir"])
            .arg(&state_a),
    );
    assert_eq!(code, 0);
    assert_eq!(listed["principals"].as_array().map(Vec::len), Some(12));

    a.stop();
    b.stop();

    // No secret ever reached stdout/stderr of any process.
    let transcript = String::from_utf8_lossy(&t.0).into_owned();
    for enrolled in clients.iter().chain([&seed, &reader, &bee]) {
        let token = fs::read_to_string(&enrolled.credential).expect("cred");
        let secret = token.trim().rsplit('.').next().expect("secret");
        assert!(
            secret.len() >= 32 && !transcript.contains(secret),
            "a secret leaked into output"
        );
        assert!(
            transcript.contains(&enrolled.token_id),
            "token ids are shown"
        );
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn expired_certificate_and_key_descriptor_negatives() {
    let root = temp_root("certs");
    let pki = material(&root);
    let mut t = Transcript::default();
    let home = root.join("install");
    let state = home.join("coord");
    fs::create_dir_all(&home).expect("home");
    let cred = enroll(
        &mut t,
        &home,
        &state,
        "m-one",
        "one",
        "worker",
        &home.join("cred"),
    );

    // An expired server certificate is refused by the client handshake.
    let expired = Server::start(
        &home,
        &state,
        &pki.join("expired.crt"),
        &pki.join("expired.key"),
    );
    let (code, refused) = t.run(&mut call_command(
        &home,
        &expired,
        &pki.join("ca-a.crt"),
        "localhost",
        &cred.credential,
        &["whoami"],
    ));
    assert_eq!(
        (code, refused["status"].as_str()),
        (5, Some("tls_failure")),
        "{refused}"
    );
    expired.stop();

    // --key-fd: a world-readable key behind the descriptor is refused...
    let loose = home.join("loose.key");
    fs::copy(pki.join("server-a.key"), &loose).expect("copy");
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("perm");
    let serve_fd = |key: &Path| {
        let mut command = Command::new("sh");
        command
            .env("HOME", &home)
            .env("KEY", key)
            .arg("-c")
            .arg("exec \"$0\" \"$@\" 3<\"$KEY\"")
            .arg(BRAIN)
            .args(["net", "serve", "--state-dir"])
            .arg(&state)
            .arg("--cert")
            .arg(pki.join("server-a.crt"))
            .args(["--key-fd", "3"]);
        command
    };
    let (code, out) = t.run(serve_fd(&loose).stdin(Stdio::null()));
    assert_eq!(
        (code, out["status"].as_str()),
        (2, Some("not_serving")),
        "{out}"
    );
    // ...a 0600 key behind it serves (stdin is /dev/null: it stops at once).
    let output = serve_fd(&pki.join("server-a.key"))
        .stdin(Stdio::null())
        .output()
        .expect("serve");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("\"serving\"") && text.contains("\"stopped\""),
        "{text}"
    );
    // Both --key and --key-fd, or neither, is a usage error.
    let output = brain(&home)
        .args(["net", "serve", "--state-dir"])
        .arg(&state)
        .arg("--cert")
        .arg(pki.join("server-a.crt"))
        .output()
        .expect("spawn");
    assert!(!output.status.success());
    let _ = fs::remove_dir_all(root);
}
