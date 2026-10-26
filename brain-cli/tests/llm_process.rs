//! `brain llm` as a real process against a fake OpenAI-compatible server on
//! 127.0.0.1 (std::net only, no external services): success, timeout fallback,
//! JSON-schema validation, env-var keys that never reach the output, and the
//! plaintext-http ban for non-loopback hosts.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

struct Home(PathBuf);

impl Home {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("brain-llm-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn brain(home: &Home, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home.path())
        .env_remove("BRAIN_DATABASE")
        .args(["--offline"])
        .args(args)
        .output()
        .expect("run brain");
    let ok = output.status.success();
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    match serde_json::from_str(&text) {
        Ok(value) => (ok, value),
        Err(_) => (
            ok,
            json!({
                "stdout": text,
                "stderr": stderr,
                "exit_ok": ok,
            }),
        ),
    }
}

fn env_with_key(home: &Home, key: &str, value: &str) -> std::collections::HashMap<String, String> {
    [
        ("HOME".to_owned(), home.path().to_string_lossy().to_string()),
        ("BRAIN_DATABASE".to_owned(), String::new()),
        ("BRAIN_OFFLINE".to_owned(), "true".to_owned()),
        (key.to_owned(), value.to_owned()),
    ]
    .into_iter()
    .collect()
}

fn brain_with_env(
    _home: &Home,
    env: &std::collections::HashMap<String, String>,
    args: &[&str],
) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .envs(env)
        .env_remove("BRAIN_DATABASE")
        .args(["--offline"])
        .args(args)
        .output()
        .expect("run brain");
    let ok = output.status.success();
    let text = String::from_utf8_lossy(&output.stdout);
    (
        ok,
        serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text})),
    )
}

/// A minimal OpenAI-compatible server on 127.0.0.1: replies to
/// POST /v1/chat/completions according to `mode`, asserts the Authorization
/// header matches the synthetic key, and never sees the key echoed back.
enum Mode {
    Success,
    Slow,
    GarbageJson,
}

fn fake_server(
    mode: Mode,
    expected_key: Option<&'static str>,
) -> (std::thread::JoinHandle<u16>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            let mut request = String::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if request.contains("\r\n\r\n") {
                    let length = request
                        .split("\r\n")
                        .find(|line| line.to_lowercase().starts_with("content-length:"))
                        .and_then(|line| line.split(':').nth(1))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let header_end = request.find("\r\n\r\n").unwrap() + 4;
                    if request.len() >= header_end + length {
                        break;
                    }
                }
            }
            if let Some(expected) = expected_key {
                let authorized = request.lines().any(|line| {
                    line.eq_ignore_ascii_case(&format!("authorization: Bearer {expected}"))
                });
                if !authorized {
                    let body = "{\"error\":\"unauthorized\"}";
                    respond(&mut stream, 401, body);
                    continue;
                }
            }
            match mode {
                Mode::Success => {
                    let body = json!({
                        "choices": [{"message": {"role": "assistant", "content": "{\"ok\": true}"}}],
                        "usage": {"completion_tokens": 7},
                    });
                    respond(&mut stream, 200, &body.to_string());
                }
                Mode::Slow => {
                    std::thread::sleep(Duration::from_millis(1200));
                    respond(&mut stream, 200, "{}");
                }
                Mode::GarbageJson => {
                    let body = json!({
                        "choices": [{"message": {"role": "assistant", "content": "not json at all"}}],
                        "usage": {"completion_tokens": 5},
                    });
                    respond(&mut stream, 200, &body.to_string());
                }
            }
        }
        port
    });
    // Give the accept loop a moment, then read the bound port back through a
    // shared handle trick: the thread returns the port on join only, so probe.
    std::thread::sleep(Duration::from_millis(100));
    (handle, port)
}

fn respond(stream: &mut TcpStream, code: u16, body: &str) {
    let response = format!(
        "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn add_provider(
    home: &Home,
    name: &str,
    base_url: &str,
    model: &str,
    extra: &[&str],
) -> (bool, Value) {
    let mut args = vec!["llm", "add", name, "--base-url", base_url, "--model", model];
    args.extend_from_slice(extra);
    brain(home, &args)
}

#[test]
fn llm_add_list_remove_round_trip() {
    let home = Home::new();
    let (ok, added) = add_provider(
        &home,
        "local",
        "http://127.0.0.1:9",
        "synthetic-model",
        &["--api-key-env", "BRAIN_SYNTHETIC_KEY", "--priority", "5"],
    );
    assert!(ok, "{added}");
    assert_eq!(added["status"], "saved");
    let text = added.to_string();
    assert!(!text.contains("sk-"), "no key material anywhere: {text}");

    let (ok, list) = brain(&home, &["llm", "list"]);
    assert!(ok);
    assert_eq!(list["count"], 1);
    assert_eq!(list["providers"][0]["name"], "local");
    assert_eq!(list["providers"][0]["priority"], 5);
    assert_eq!(list["providers"][0]["api_key_env"], "BRAIN_SYNTHETIC_KEY");

    let (ok, removed) = brain(&home, &["llm", "remove", "local"]);
    assert!(ok, "{removed}");
    assert_eq!(removed["status"], "removed");
    let (_, list) = brain(&home, &["llm", "list"]);
    assert_eq!(list["count"], 0);

    let (ok, missing) = brain(&home, &["llm", "remove", "local"]);
    assert!(!ok, "removing an absent provider fails: {missing}");
}

#[test]
fn llm_test_success_reports_counters_without_response_text() {
    let home = Home::new();
    let (_handle, port) = fake_server(Mode::Success, Some("synthetic-key-value"));
    let base_url = format!("http://127.0.0.1:{port}");
    let (ok, _) = add_provider(
        &home,
        "local",
        &base_url,
        "synthetic-model",
        &["--api-key-env", "BRAIN_SYNTHETIC_KEY"],
    );
    assert!(ok);

    let env = env_with_key(&home, "BRAIN_SYNTHETIC_KEY", "synthetic-key-value");
    let (ok, report) = brain_with_env(
        &home,
        &env,
        &["llm", "test", "local", "--timeout-seconds", "5"],
    );
    assert!(ok, "{report}");
    assert_eq!(report["status"], "ok");
    let attempt = &report["attempts"][0];
    assert_eq!(attempt["ok"], true);
    assert_eq!(attempt["json_valid"], true);
    assert!(attempt["elapsed_ms"].as_u64().unwrap() < 5000);
    assert!(attempt["tokens_per_second"].as_f64().unwrap() > 0.0);
    assert!(
        !report.to_string().contains("synthetic-key-value"),
        "the API key never reaches the output: {report}"
    );
    assert!(
        !report.to_string().contains("\"ok\": true}"),
        "the response text is not printed without --show: {report}"
    );
}

#[test]
fn timeout_falls_back_to_the_next_provider() {
    let home = Home::new();
    let (_slow_handle, slow_port) = fake_server(Mode::Slow, None);
    let (_fast_handle, fast_port) = fake_server(Mode::Success, None);
    let (ok, _) = add_provider(
        &home,
        "slow",
        &format!("http://127.0.0.1:{slow_port}"),
        "synthetic-model",
        &["--priority", "1"],
    );
    assert!(ok);
    let (ok, _) = add_provider(
        &home,
        "fast",
        &format!("http://127.0.0.1:{fast_port}"),
        "synthetic-model",
        &["--priority", "2"],
    );
    assert!(ok);

    let (ok, report) = brain(&home, &["llm", "test", "--timeout-seconds", "1"]);
    // Server threads stay in their accept loops on purpose: the test process
    // exits and reclaims them, joining here would block forever.
    assert!(ok, "{report}");
    assert_eq!(report["status"], "ok");
    let attempts = report["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["provider"], "slow");
    assert_eq!(attempts[0]["ok"], false, "the slow provider timed out");
    assert_eq!(attempts[1]["provider"], "fast");
    assert_eq!(attempts[1]["ok"], true);
    let answered = &report["answered_by"];
    assert_eq!(answered["provider"], "fast");
}

#[test]
fn invalid_json_for_schema_is_an_error_not_success() {
    let home = Home::new();
    let (_handle, port) = fake_server(Mode::GarbageJson, None);
    let (ok, _) = add_provider(
        &home,
        "local",
        &format!("http://127.0.0.1:{port}"),
        "synthetic-model",
        &[],
    );
    assert!(ok);
    let (ok, report) = brain(&home, &["llm", "test", "local", "--timeout-seconds", "5"]);
    assert!(
        ok,
        "the command runs; the report says unavailable: {report}"
    );
    assert_eq!(report["status"], "unavailable");
    let attempt = &report["attempts"][0];
    assert_eq!(attempt["ok"], false);
    assert_eq!(attempt["json_valid"], false);
    assert!(attempt["error"].as_str().unwrap().contains("schema"));
}

#[test]
fn missing_api_key_env_is_reported_and_plaintext_http_off_loopback_is_refused() {
    let home = Home::new();
    let (_handle, port) = fake_server(Mode::Success, None);
    let (ok, _) = add_provider(
        &home,
        "keyed",
        &format!("http://127.0.0.1:{port}"),
        "synthetic-model",
        &["--api-key-env", "BRAIN_ABSENT_SYNTHETIC_KEY"],
    );
    assert!(ok);
    let (ok, report) = brain(&home, &["llm", "test", "keyed", "--timeout-seconds", "2"]);
    assert!(ok);
    assert_eq!(report["status"], "unavailable");
    let error = report["attempts"][0]["error"].as_str().unwrap();
    assert!(error.contains("BRAIN_ABSENT_SYNTHETIC_KEY"), "{error}");
    assert!(error.contains("never stored"), "{error}");

    // Plaintext http to a non-loopback host is refused at `llm add` time.
    let (ok, refused) = add_provider(
        &home,
        "leaky",
        "http://10.0.0.9:8080",
        "synthetic-model",
        &[],
    );
    assert!(
        !ok,
        "plaintext http off loopback must be refused: {refused}"
    );
    assert!(
        refused["stderr"]
            .as_str()
            .unwrap_or_default()
            .contains("https://"),
        "{refused}"
    );
}
