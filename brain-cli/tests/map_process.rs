//! `brain map` and the `map_*` MCP tools as real processes on a synthetic,
//! isolated HOME. This covers our stdio server, not any particular LLM client.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MACHINE: &str = "m-0123456789abcdef0123456789abcdef";

fn isolated_home(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "brain-map-process-{tag}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create isolated home");
    root
}

fn brain(home: &Path, agent: &str, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_brain"));
    command
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_AGENT")
        .env("BRAIN_OFFLINE", "true")
        .env("BRAIN_MACHINE_ID", MACHINE)
        .args([
            "--database",
            home.join(".brain/brain.sqlite").to_str().expect("utf8"),
            "--agent",
            agent,
        ])
        .args(args);
    command
}

fn run(home: &Path, args: &[&str]) -> (Output, Value) {
    let output = brain(home, "codex", args).output().expect("spawn brain");
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout is not JSON: {} / {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, value)
}

fn ok(home: &Path, args: &[&str]) -> Value {
    let (output, value) = run(home, args);
    assert!(output.status.success(), "{args:?} failed: {value}");
    value
}

fn section<'a>(packet: &'a Value, name: &str) -> &'a Vec<Value> {
    packet["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()["items"]
        .as_array()
        .unwrap()
}

/// Project A: task 1 completed. Project B: task 2 failed.
fn history(home: &Path) {
    for (project, title, state) in [("A", "alpha", "completed"), ("B", "beta", "failed")] {
        let task = ok(home, &["work", "add", project, title])["task"]["id"]
            .as_i64()
            .unwrap()
            .to_string();
        let claim = ok(home, &["work", "claim", &task]);
        let (id, fence) = (
            claim["claim"]["claim_id"].to_string(),
            claim["claim"]["fence"].to_string(),
        );
        ok(home, &["work", "state", &id, &fence, state]);
    }
}

#[test]
fn reads_never_create_the_map_and_packets_fall_back_truthfully() {
    let home = isolated_home("absent");
    let status = ok(&home, &["map", "status"]);
    assert_eq!(
        (status["map"].as_str(), status["coordinator"].as_str()),
        (Some("not_configured"), Some("not_configured"))
    );
    let (out, packet) = run(&home, &["map", "packet"]);
    assert!(out.status.success());
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(raw.len() <= 2_048);
    assert_eq!(packet["bytes"].as_u64().unwrap() as usize, raw.len());
    assert_eq!(packet["map"], "not_configured");
    assert_eq!(
        section(&packet, "SCOPE")[0]["source"],
        "coordinator_only+memory"
    );
    for args in [
        &["map", "timeline"][..],
        &["map", "links"],
        &["map", "ingest-coordinator"],
    ] {
        let (out, err) = run(&home, args);
        assert!(!out.status.success());
        assert_eq!(err["error_kind"], "not_configured", "{args:?}");
    }
    assert!(
        !home.join(".brain/map").exists(),
        "no map directory was created"
    );
    assert!(
        !home.join(".brain/machine-id").exists(),
        "no installation id was created"
    );
}

#[test]
fn init_ingest_timeline_and_a_project_scoped_packet() {
    let home = isolated_home("flow");
    history(&home);
    let status = ok(&home, &["map", "init"]);
    assert_eq!(
        (status["map"].as_str(), status["install"].as_str()),
        (Some("ready"), Some(MACHINE))
    );
    let ingest = ok(&home, &["map", "ingest-coordinator"]);
    assert!(
        ingest["appended"].as_u64().unwrap() >= 6 && ingest["caught_up"] == true,
        "{ingest}"
    );
    assert_eq!(
        ok(&home, &["map", "ingest-coordinator"])["appended"],
        0,
        "idempotent"
    );
    let timeline = ok(&home, &["map", "timeline", "--task", "1"]);
    let outcomes: Vec<&str> = timeline["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["outcome"].as_str().unwrap())
        .filter(|o| !o.is_empty())
        .collect();
    assert_eq!(outcomes, vec!["claimed_completed"]);
    let (out, packet) = run(
        &home,
        &["map", "packet", "--project", "A", "--tier", "strong"],
    );
    assert!(out.status.success());
    let raw = String::from_utf8(out.stdout).unwrap();
    assert_eq!(packet["bytes"].as_u64().unwrap() as usize, raw.len());
    assert_eq!(packet["map"], "ready");
    assert_eq!(section(&packet, "SCOPE")[0]["project"], "project:default/A");
    let tasks: Vec<&str> = section(&packet, "OUTCOMES")
        .iter()
        .map(|o| o["task"].as_str().unwrap())
        .collect();
    assert_eq!(tasks, vec!["task:default/1"]);
    assert_eq!(section(&packet, "OUTCOMES")[0]["label"], "claimed");
    assert!(
        !raw.contains("task:default/2") && !raw.contains("beta"),
        "project B leaked"
    );
    assert_eq!(packet["out_of_scope"]["OUTCOMES"], 1);
    let whole = ok(&home, &["map", "packet", "--tier", "strong"]);
    assert_eq!(section(&whole, "OUTCOMES").len(), 2);
    // Negative paths: stable errors, no overrun, no authority flag.
    for (args, kind) in [
        (&["map", "packet", "--budget", "10"][..], "invalid_input"),
        (
            &["map", "packet", "--tier", "weak", "--budget", "5000"],
            "invalid_input",
        ),
        (&["map", "packet", "--project", "../etc"], "invalid_input"),
    ] {
        let (out, err) = run(&home, args);
        assert!(!out.status.success());
        assert_eq!(err["error_kind"], kind, "{args:?}");
    }
    for flag in ["--trusted-user", "--install", "--path", "--map-database"] {
        let out = brain(&home, "codex", &["map", "packet", flag, "x"])
            .output()
            .unwrap();
        assert!(
            !out.status.success() && out.stdout.is_empty(),
            "{flag} accepted"
        );
    }
    let status = ok(&home, &["map", "status"]);
    assert!(status["events"].as_i64().unwrap() >= 6);
    let mode = |p: PathBuf| {
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(p).unwrap().permissions()) & 0o777
    };
    assert_eq!(
        (
            mode(home.join(".brain/map")),
            mode(home.join(".brain/map/map.sqlite"))
        ),
        (0o700, 0o600)
    );
}

#[test]
fn stdio_mcp_lists_and_scopes_map_tools_and_refuses_authority_inputs() {
    let home = isolated_home("mcp");
    history(&home);
    ok(&home, &["map", "init"]);
    ok(&home, &["map", "ingest-coordinator"]);
    let calls = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"map_status","arguments":{}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"map_packet","arguments":{"project":"A","tier":"strong","budget_bytes":4000}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"map_packet","arguments":{"install":"evil"}}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"map_packet","arguments":{"path":"/etc/passwd"}}}),
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"map_ingest_coordinator","arguments":{"source":"/tmp/x.sqlite"}}}),
        json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"map_timeline","arguments":{"space":"default","task_id":2}}}),
        json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"map_packet","arguments":{"space":"elsewhere"}}}),
    ];
    let mut child = brain(&home, "claude", &["serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for call in &calls {
            writeln!(stdin, "{call}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), calls.len());
    let result = |i: usize| replies[i]["result"]["structuredContent"].clone();
    let names: Vec<String> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    for wanted in [
        "map_status",
        "map_timeline",
        "map_packet",
        "map_ingest_coordinator",
    ] {
        assert!(names.iter().any(|n| n == wanted), "{wanted}");
    }
    assert!(!names.iter().any(|n| n.starts_with("map_")
        && (n.contains("confirm") || n.contains("observe") || n.contains("reject"))));
    assert_eq!(result(2)["map"], "ready");
    let packet = result(3);
    let raw = packet["packet_json"].as_str().unwrap();
    assert_eq!(packet["packet_bytes"].as_u64().unwrap() as usize, raw.len());
    assert!(raw.len() <= 4_000);
    let parsed: Value = serde_json::from_str(raw).unwrap();
    assert!(
        section(&parsed, "OUTCOMES")
            .iter()
            .all(|o| o["task"] == "task:default/1")
    );
    for i in [4, 5, 6] {
        assert_eq!(result(i)["error_kind"], "invalid_input", "{}", replies[i]);
        assert_eq!(replies[i]["result"]["isError"], true);
    }
    let timeline = result(7);
    let failed: Vec<&str> = timeline["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["outcome"].as_str().unwrap())
        .filter(|o| !o.is_empty())
        .collect();
    assert_eq!(failed, vec!["claimed_failed"]);
    let other: Value = serde_json::from_str(result(8)["packet_json"].as_str().unwrap()).unwrap();
    assert_eq!(other["sections"][0]["items"][0]["space"], "elsewhere");
    assert!(
        section(&other, "OUTCOMES").is_empty(),
        "another space never merges in"
    );
}

#[test]
fn links_page_through_every_row_with_the_returned_cursor() {
    use brain_core::map::{Authority, EventDraft, MapStore};
    let home = isolated_home("links");
    ok(&home, &["map", "init"]);
    {
        let mut m = MapStore::open(&home.join(".brain/map/map.sqlite"), MACHINE).unwrap();
        for i in 0..5 {
            let d = EventDraft {
                kind: "link_proposed".into(),
                space: "default".into(),
                subject: "project:default/A".into(),
                relation: "has_session".into(),
                object: format!("archive-session:codex/s{i}"),
                basis: "agent_claim".into(),
                asserted_kind: "agent".into(),
                client: "codex".into(),
                agent: "codex".into(),
                occurred_at_source: "unknown".into(),
                confidence_milli: 700,
                ..EventDraft::default()
            };
            m.append(d, Authority::Unverified).unwrap();
        }
    }
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut args = vec![
            "map",
            "links",
            "--limit",
            "2",
            "--subject",
            "project:default/A",
        ];
        if let Some(c) = &cursor {
            args.extend(["--cursor", c.as_str()]);
        }
        let page = ok(&home, &args);
        seen.extend(
            page["links"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l["object"].as_str().unwrap().to_owned()),
        );
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    let want: Vec<String> = (0..5)
        .map(|i| format!("archive-session:codex/s{i}"))
        .collect();
    assert_eq!(seen, want, "every link reachable exactly once");
    let (out, err) = run(&home, &["map", "links", "--cursor", "not-a-cursor"]);
    assert!(!out.status.success());
    assert_eq!(err["error_kind"], "invalid_input");
}
