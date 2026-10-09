//! Project/scope tag normalization as a real stdio MCP server process on a
//! synthetic, isolated HOME. `project:<name>` and `scope:space` markers must
//! survive storage so project-scoped map packets can find the memory again;
//! malformed markers are rejected, never silently widened or lowercased.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MACHINE: &str = "m-0123456789abcdef0123456789abcdef";

fn isolated_home(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "brain-project-memory-process-{tag}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create isolated home");
    root
}

/// One stdio MCP conversation against `brain serve`.
fn mcp(home: &Path, space: &str, agent: &str, calls: &[Value]) -> Vec<Value> {
    let mut prefixed = vec![
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ];
    prefixed.extend_from_slice(calls);
    let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_AGENT")
        .env("BRAIN_OFFLINE", "true")
        .env("BRAIN_MACHINE_ID", MACHINE)
        .args([
            "--database",
            home.join(".brain/brain.sqlite").to_str().expect("utf8"),
            "--space",
            space,
            "--agent",
            agent,
            "serve",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        for call in &prefixed {
            writeln!(stdin, "{call}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

fn content(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn project_marker_survives_storage_and_scopes_packets() {
    let home = isolated_home("project-a");
    let replies = mcp(
        &home,
        "canary",
        "probe-tags",
        &[
            call(
                2,
                "memory_write",
                json!({"space": "canary", "kind": "decision", "text": "CANARY-PROJECT-A-MARKER chose alpha", "tags": ["project:A", "FreeForm Tag"]}),
            ),
            call(
                3,
                "memory_write",
                json!({"space": "canary", "kind": "decision", "text": "CANARY-PROJECT-B-MARKER chose beta", "tags": ["project:B"]}),
            ),
            call(
                4,
                "map_packet",
                json!({"space": "canary", "project": "A", "budget_bytes": 2048, "memory_query": "CANARY-PROJECT"}),
            ),
            call(
                5,
                "map_packet",
                json!({"space": "canary", "project": "B", "budget_bytes": 2048, "memory_query": "CANARY-PROJECT"}),
            ),
            call(
                6,
                "map_packet",
                json!({"space": "canary", "budget_bytes": 2048, "memory_query": "CANARY-PROJECT"}),
            ),
            call(
                7,
                "map_packet",
                json!({"space": "canary", "budget_bytes": 2048, "memory_query": "qzjxv884422absent"}),
            ),
        ],
    );

    // Writes keep the reserved colon and the canonical project case; the
    // freeform tag keeps the historic lowercased space-join behaviour.
    let write_a = content(&replies[1]);
    assert_eq!(write_a["status"], "ok", "{write_a}");
    let read = mcp(
        &home,
        "canary",
        "probe-tags",
        &[call(
            2,
            "memory_read",
            json!({"space": "canary", "ids": [write_a["id"].as_str().unwrap()]}),
        )],
    );
    let record = &content(&read[1])["records"][0];
    let tags = record["tags"].as_array().unwrap();
    assert!(
        tags.iter().any(|t| t == "project:A"),
        "canonical marker retained exactly: {tags:?}"
    );
    assert!(
        tags.iter().any(|t| t == "freeform tag"),
        "freeform tags unchanged: {tags:?}"
    );

    let packet = |i: usize| -> Value {
        let envelope = content(&replies[i]);
        let raw = envelope["packet_json"]
            .as_str()
            .unwrap_or_else(|| panic!("packet reply {i}: {envelope}"));
        serde_json::from_str(raw).unwrap()
    };
    let decisions = |p: &Value| -> Vec<String> {
        p["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "DECISIONS")
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["text"].as_str().unwrap().to_owned())
            .collect()
    };

    let a = packet(3);
    assert!(
        decisions(&a)
            .iter()
            .any(|t| t.contains("CANARY-PROJECT-A-MARKER")),
        "{a}"
    );
    assert!(
        !decisions(&a)
            .iter()
            .any(|t| t.contains("CANARY-PROJECT-B-MARKER")),
        "project B leaked into project A packet: {a}"
    );
    assert_eq!(
        a["sections"][3]["items"][0]["scope"], "project:canary/A",
        "canonical case A, not lowered: {a}"
    );
    let b = packet(4);
    assert!(
        decisions(&b)
            .iter()
            .any(|t| t.contains("CANARY-PROJECT-B-MARKER")),
        "{b}"
    );
    assert!(
        !decisions(&b)
            .iter()
            .any(|t| t.contains("CANARY-PROJECT-A-MARKER")),
        "project A leaked into project B packet: {b}"
    );
    let unscoped = packet(5);
    let texts = decisions(&unscoped).join("\n");
    assert!(
        texts.contains("CANARY-PROJECT-A-MARKER") && texts.contains("CANARY-PROJECT-B-MARKER"),
        "{unscoped}"
    );
    assert!(
        decisions(&packet(6)).is_empty(),
        "query that hits nothing yields no memories"
    );

    // scope:space is kept verbatim and lands in every project packet.
    let replies = mcp(
        &home,
        "canary",
        "probe-tags",
        &[
            call(
                2,
                "memory_write",
                json!({"space": "canary", "kind": "gotcha", "text": "CANARY-SPACE-GOTCHA shared trap", "tags": ["scope:space"]}),
            ),
            call(
                3,
                "map_packet",
                json!({"space": "canary", "project": "A", "budget_bytes": 2048, "memory_query": "CANARY-SPACE"}),
            ),
        ],
    );
    assert_eq!(
        content(&replies[1])["status"],
        "ok",
        "{}",
        content(&replies[1])
    );
    let envelope = content(&replies[2]);
    let shared: Value =
        serde_json::from_str(envelope["packet_json"].as_str().expect("space packet")).unwrap();
    let gotchas = shared["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "GOTCHAS")
        .unwrap()["items"]
        .as_array()
        .unwrap();
    assert!(
        gotchas
            .iter()
            .any(|i| i["text"].as_str().unwrap().contains("CANARY-SPACE-GOTCHA")),
        "{shared}"
    );
    assert_eq!(gotchas[0]["scope"], "space_shared");
}

#[test]
fn malformed_markers_are_rejected_not_widened() {
    let home = isolated_home("rejects");
    let replies = mcp(
        &home,
        "canary",
        "probe-tags",
        &[
            call(
                2,
                "memory_write",
                json!({"space": "canary", "text": "unicode project", "tags": ["project:café"]}),
            ),
            call(
                3,
                "memory_write",
                json!({"space": "canary", "text": "path escape project", "tags": ["project:../etc"]}),
            ),
            call(
                4,
                "memory_write",
                json!({"space": "canary", "text": "empty project", "tags": ["project:"]}),
            ),
            call(
                5,
                "memory_write",
                json!({"space": "canary", "text": "unknown scope", "tags": ["scope:global"]}),
            ),
            call(
                6,
                "memory_write",
                json!({"space": "canary", "text": "still works", "tags": ["ordinary"]}),
            ),
        ],
    );
    for reply in &replies[1..=4] {
        let outcome = content(reply);
        assert_eq!(
            outcome["status"], "error",
            "malformed marker must fail, not widen scope: {outcome}"
        );
    }
    assert_eq!(
        content(&replies[5])["status"],
        "ok",
        "{}",
        content(&replies[5])
    );

    // Nothing the rejected writes said is retrievable.
    let read = mcp(
        &home,
        "canary",
        "probe-tags",
        &[call(
            2,
            "memory_search",
            json!({"space": "canary", "query": "project"}),
        )],
    );
    let hits = content(&read[1]);
    let blob = hits.to_string();
    assert!(
        !blob.contains("unicode project") && !blob.contains("path escape project"),
        "{blob}"
    );
}
