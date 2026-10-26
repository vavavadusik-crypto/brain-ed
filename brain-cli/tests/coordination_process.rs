//! The coordinator across real processes: ten `brain` clients with distinct
//! installation ids race on one isolated database. This proves the local
//! concurrency semantics, not a network deployment on ten computers.

#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn isolated_home(_tag: &str) -> PathBuf {
    let _nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = test_support::temp_path("brain-coord-process");
    fs::create_dir_all(root.join(".brain")).expect("create isolated home");
    root
}

fn command(home: &Path, agent: &str, machine: Option<&str>, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brain"));
    command
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_AGENT")
        .env("BRAIN_OFFLINE", "true")
        .args([
            "--database",
            home.join(".brain/brain.sqlite").to_str().expect("utf8"),
            "--agent",
            agent,
            "work",
        ])
        .args(args);
    match machine {
        Some(machine) => command.env("BRAIN_MACHINE_ID", machine),
        None => command.env_remove("BRAIN_MACHINE_ID"),
    };
    command
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn run(home: &Path, agent: &str, machine: Option<&str>, args: &[&str]) -> (bool, Value) {
    let output = command(home, agent, machine, args)
        .output()
        .expect("spawn brain");
    (output.status.success(), json_of(&output))
}

#[test]
fn ten_processes_race_for_one_task_and_exactly_one_owns_it() {
    let home = isolated_home("race");
    let (ok, added) = run(
        &home,
        "seed",
        Some("m-seed"),
        &["add", "proj", "contended task"],
    );
    assert!(ok, "{added}");
    let id = added["task"]["id"].as_i64().expect("task id").to_string();

    let children = (0..10)
        .map(|n| {
            command(
                &home,
                &format!("agent{n}"),
                Some(&format!("m-client-{n:02}")),
                &["claim", &id, "--ttl", "120"],
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn client")
        })
        .collect::<Vec<_>>();
    let outputs = children
        .into_iter()
        .map(|child| child.wait_with_output().expect("wait"))
        .collect::<Vec<_>>();
    let winners = outputs
        .iter()
        .filter(|o| o.status.success())
        .collect::<Vec<_>>();
    assert_eq!(winners.len(), 1, "exactly one owner");
    for loser in outputs.iter().filter(|o| !o.status.success()) {
        assert_eq!(json_of(loser)["error_kind"], "busy");
    }
    let winner = json_of(winners[0]);
    let (_, board) = run(
        &home,
        "observer",
        Some("m-observer"),
        &["board", "--project", "proj"],
    );
    assert_eq!(
        board["tasks"][0]["owner"]["machine"],
        winner["claim"]["machine"]
    );
    assert_eq!(board["tasks"][0]["state"], "active");

    // Unrelated project proceeds independently, path claims too.
    let (ok, other) = run(
        &home,
        "agent0",
        Some("m-client-00"),
        &["add", "other-proj", "independent"],
    );
    assert!(ok);
    let other_id = other["task"]["id"].as_i64().unwrap().to_string();
    assert!(run(&home, "agent1", Some("m-client-01"), &["claim", &other_id]).0);
    assert!(
        run(
            &home,
            "agent2",
            Some("m-client-02"),
            &["claim-path", "proj", "src/lib.rs"]
        )
        .0
    );
    let (ok, conflict) = run(
        &home,
        "agent3",
        Some("m-client-03"),
        &["claim-path", "proj", "src"],
    );
    assert!(!ok);
    assert_eq!(conflict["error_kind"], "conflict");
    assert!(
        run(
            &home,
            "agent3",
            Some("m-client-03"),
            &["claim-path", "other-proj", "src"]
        )
        .0
    );

    // Inputs a shell could produce are refused, not coerced.
    for ttl in ["-5", "0", "1.5", "NaN", "inf", "999999999"] {
        let output = command(
            &home,
            "agent4",
            Some("m-client-04"),
            &["claim", &other_id, "--ttl", ttl],
        )
        .output()
        .unwrap();
        assert!(!output.status.success(), "ttl {ttl} must be refused");
    }
    let _ = fs::remove_dir_all(home);
}

#[test]
fn installation_id_is_stable_and_not_a_model_name() {
    let home = isolated_home("identity");
    let (ok, first) = run(&home, "claude-code", None, &["whoami"]);
    assert!(ok, "{first}");
    let (_, second) = run(&home, "codex", None, &["whoami"]);
    assert_eq!(
        first["machine"], second["machine"],
        "one installation, one id"
    );
    assert_ne!(first["agent"], second["agent"]);
    let machine = first["machine"].as_str().unwrap();
    assert!(machine.starts_with("m-") && machine.len() == 34);
    assert_eq!(second["machine_source"], "installation id");
    let (_, overridden) = run(&home, "codex", Some("m-sim-7"), &["whoami"]);
    assert_eq!(overridden["machine"], "m-sim-7");
    assert!(
        overridden["machine_source"]
            .as_str()
            .unwrap()
            .contains("override")
    );
    let (ok, bad) = run(&home, "codex", Some("Claude Opus"), &["whoami"]);
    assert!(!ok);
    assert_eq!(bad["error_kind"], "invalid_input");
    // Reads before any write answer honestly.
    let (ok, missing) = run(&home, "codex", None, &["board"]);
    assert!(!ok);
    assert_eq!(missing["error_kind"], "not_configured");
    let _ = fs::remove_dir_all(home);
}

fn mcp(home: &Path, agent: &str, machine: &str, calls: &[(&str, Value)]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env("BRAIN_MACHINE_ID", machine)
        .env("BRAIN_OFFLINE", "true")
        .env_remove("BRAIN_DATABASE")
        .args([
            "--database",
            home.join(".brain/brain.sqlite").to_str().unwrap(),
            "--agent",
            agent,
            "serve",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    {
        let stdin = child.stdin.as_mut().unwrap();
        for (n, (name, args)) in calls.iter().enumerate() {
            let request = json!({"jsonrpc":"2.0","id":n + 1,"method":"tools/call","params":{"name":name,"arguments":args}});
            writeln!(stdin, "{request}").unwrap();
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let response: Value = serde_json::from_str(line).unwrap();
            let text = response["result"]["content"][0]["text"]
                .as_str()
                .expect("tool text")
                .to_owned();
            serde_json::from_str(&text).expect("tool JSON")
        })
        .collect()
}

#[test]
fn mcp_tools_claim_fence_handoff_and_refuse_bad_numbers() {
    let home = isolated_home("mcp");
    let answers = mcp(
        &home,
        "codex",
        "m-aaa",
        &[
            (
                "work_add",
                json!({"project":"p","title":"ship it","request_id":"r1"}),
            ),
            (
                "work_add",
                json!({"project":"p","title":"ship it","request_id":"r1"}),
            ),
            ("work_claim", json!({"task_id":1,"ttl_secs":60})),
            ("work_claim", json!({"task_id":1,"ttl_secs":1.5})),
            ("work_claim", json!({"task_id":1,"ttl_secs":-60})),
            ("work_claim", json!({"task_id":"1"})),
            ("work_board", json!({"project":"p","include_claims":true})),
        ],
    );
    assert_eq!(answers[0]["task"]["id"], 1);
    assert_eq!(answers[1]["replayed"], true);
    let fence = answers[2]["claim"]["fence"].as_i64().unwrap();
    let claim = answers[2]["claim"]["claim_id"].as_i64().unwrap();
    for refused in &answers[3..6] {
        assert_eq!(refused["error_kind"], "invalid_input", "{refused}");
    }
    assert_eq!(answers[6]["you"]["machine"], "m-aaa");
    assert_eq!(answers[6]["claims"].as_array().unwrap().len(), 1);
    assert!(
        answers[6]["trust"]
            .as_str()
            .unwrap()
            .contains("agent_supplied")
    );

    let other = mcp(
        &home,
        "hermes",
        "m-bbb",
        &[
            ("work_claim", json!({"task_id":1})),
            (
                "work_set_state",
                json!({"claim_id":claim,"fence":fence,"state":"completed"}),
            ),
        ],
    );
    assert_eq!(other[0]["error_kind"], "busy");
    assert_eq!(other[1]["error_kind"], "not_owner");

    let handed = mcp(
        &home,
        "codex",
        "m-aaa",
        &[
            (
                "work_handoff",
                json!({"claim_id":claim,"fence":fence,"to_agent":"hermes","to_machine":"m-bbb","note":"over to you"}),
            ),
            ("work_renew", json!({"claim_id":claim,"fence":fence})),
        ],
    );
    let new_fence = handed[0]["claim"]["fence"].as_i64().unwrap();
    assert!(new_fence > fence);
    assert_eq!(handed[1]["error_kind"], "stale_fence");

    let finished = mcp(
        &home,
        "hermes",
        "m-bbb",
        &[
            ("work_claim", json!({"task_id":1})),
            (
                "work_set_state",
                json!({"claim_id":handed[0]["claim"]["claim_id"],"fence":new_fence,"state":"completed","note":"done"}),
            ),
            ("work_show", json!({"task_id":1})),
        ],
    );
    assert_eq!(finished[0]["already_owned"], true);
    assert_eq!(finished[2]["task"]["state"], "completed");
    let kinds = finished[2]["recent_events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["created", "claimed", "handed_off", "completed"]);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn orphaned_task_is_ready_to_read_only_pollers_after_its_owner_stops() {
    let home = isolated_home("orphan");
    let (ok, added) = run(&home, "codex", Some("m-aaa"), &["add", "p", "orphan"]);
    assert!(ok, "{added}");
    let id = added["task"]["id"].as_i64().unwrap();
    assert!(
        run(
            &home,
            "codex",
            Some("m-aaa"),
            &["claim", &id.to_string(), "--ttl", "10"]
        )
        .0
    );
    // The owner stops; nobody writes again. Only read-only polling follows.
    std::thread::sleep(std::time::Duration::from_secs(11));
    let (ok, ready) = run(&home, "hermes", Some("m-bbb"), &["board", "--ready"]);
    assert!(ok, "{ready}");
    assert_eq!(
        ready["tasks"][0]["id"], id,
        "orphan must be offered as ready: {ready}"
    );
    assert_eq!(ready["tasks"][0]["owner"], Value::Null);
    let (_, queued) = run(
        &home,
        "hermes",
        Some("m-bbb"),
        &["board", "--state", "queued"],
    );
    assert_eq!(queued["tasks"].as_array().unwrap().len(), 1);
    let (_, active) = run(
        &home,
        "hermes",
        Some("m-bbb"),
        &["board", "--state", "active"],
    );
    assert_eq!(active["tasks"].as_array().unwrap().len(), 0);
    assert_eq!(active["counts"]["active"], 0);
    assert_eq!(active["counts"]["queued"], 1);
    // And it can actually be recovered.
    let (ok, taken) = run(&home, "hermes", Some("m-bbb"), &["claim", &id.to_string()]);
    assert!(ok, "{taken}");
    assert_eq!(taken["previous_claim"]["ended"], "expired");
    let _ = fs::remove_dir_all(home);
}
