//! `map_packet` with `compact_response: true` through the real `brain serve`
//! stdio MCP on a synthetic, isolated HOME. Every budget assertion measures
//! the UTF-8 bytes of `result.content[0].text` exactly as the client
//! receives it — the whole content text, envelope included — not the inner
//! packet. JSON-RPC transport escaping of that text is outside the budget.

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
        "brain-compact-packet-{tag}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("create isolated home");
    root
}

fn brain(home: &Path) -> Command {
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
            "probe-compact",
        ]);
    command
}

fn share(home: &Path, state: &Path, args: &[&str]) {
    let output = brain(home)
        .args(["net", "share"])
        .args(&args[..1])
        .args(["--state-dir", state.to_str().expect("utf8")])
        .args(&args[1..])
        .output()
        .expect("run share");
    let document: Value = serde_json::from_slice(&output.stdout).expect("share json");
    assert_eq!(document["status"], "ok", "{document}");
}

/// One stdio MCP conversation; replies in order, `initialize` first.
fn mcp(home: &Path, share_dir: Option<&Path>, calls: &[Value]) -> Vec<Value> {
    let mut command = brain(home);
    command.args(["--space", "canary"]);
    if let Some(dir) = share_dir {
        command.args(["--share-state-dir", dir.to_str().expect("utf8")]);
    }
    let mut child = command
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})
        )
        .unwrap();
        for call in calls {
            writeln!(stdin, "{call}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(replies.len(), calls.len() + 1);
    replies
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})
}

/// The content text exactly as received, and its parsed form.
fn content_text(reply: &Value) -> (String, Value) {
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text content")
        .to_owned();
    let parsed = serde_json::from_str(&text).expect("content text is valid JSON");
    (text, parsed)
}

fn items(packet: &Value) -> Vec<Value> {
    packet["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .flat_map(|s| s["items"].as_array().cloned().expect("items"))
        .collect()
}

fn packet_args(extra: Value) -> Value {
    let mut args = json!({
        "space": "canary", "project": "alpha", "memory_query": "lighthouse", "compact_response": true
    });
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

/// Synthetic data: many long multilingual alpha memories full of characters
/// that JSON must escape, a beta project, and shared facts (one retracted).
fn seed(home: &Path, state: &Path) {
    let mut writes = Vec::new();
    for i in 0..24 {
        let text = format!(
            "lighthouse {i} «маяк» 灯塔 🗼 \"quoted\" back\\slash\ttab\nnewline \u{7f} — {}",
            "Ελληνικά العربية हिन्दी ".repeat(6)
        );
        writes.push(call(
            i + 1,
            "memory_write",
            json!({"space": "canary", "kind": if i % 2 == 0 { "fact" } else { "note" },
                   "text": text, "tags": ["project:alpha"]}),
        ));
    }
    writes.push(call(
        100,
        "memory_write",
        json!({"space": "canary", "kind": "fact", "text": "lighthouse beta-only secret plan",
               "tags": ["project:beta"]}),
    ));
    for reply in &mcp(home, None, &writes)[1..] {
        assert_eq!(content_text(reply).1["status"], "ok", "{reply}");
    }
    fs::create_dir_all(state).unwrap();
    share(home, state, &["enable", "--space", "canary"]);
    share(
        home,
        state,
        &[
            "publish",
            "--space",
            "canary",
            "--fact-id",
            "S1",
            "--payload",
            r#"{"project":"alpha","note":"lighthouse \"shared\" ✓"}"#,
            "--claim",
            "shared lighthouse «fact»",
            "--claimed-agent",
            "claimed-hermes",
        ],
    );
    share(
        home,
        state,
        &[
            "publish",
            "--space",
            "canary",
            "--fact-id",
            "S2",
            "--payload",
            r#"{"project":"alpha","note":"lighthouse to retract"}"#,
            "--claim",
            "retracted lighthouse fact",
        ],
    );
    share(
        home,
        state,
        &["retract", "--space", "canary", "--fact-id", "S2"],
    );
}

#[test]
fn compact_content_text_is_bounded_valid_typed_and_isolated() {
    let home = isolated_home("bounded");
    let state = home.join("net-state");
    seed(&home, &state);

    let cases: Vec<(&str, Value, usize)> = vec![
        ("weak default", json!({}), 2_048),
        (
            "weak max",
            json!({"tier": "weak", "budget_bytes": 4_096}),
            4_096,
        ),
        ("strong default", json!({"tier": "strong"}), 12_288),
        (
            "strong max",
            json!({"tier": "strong", "budget_bytes": 32_768}),
            32_768,
        ),
        ("tiny", json!({"budget_bytes": 1_300}), 1_300),
    ];
    let calls: Vec<Value> = cases
        .iter()
        .enumerate()
        .map(|(i, (_, extra, _))| call(i as i64 + 1, "map_packet", packet_args(extra.clone())))
        .collect();
    let replies = mcp(&home, Some(&state), &calls);
    let again = mcp(&home, Some(&state), &calls);
    let mut previous_items = 0;
    for (i, (name, _, budget)) in cases.iter().enumerate() {
        let reply = &replies[i + 1];
        let (text, body) = content_text(reply);
        assert!(text.len() <= *budget, "{name}: {} > {budget}", text.len());
        assert_eq!(reply["result"]["isError"], false, "{name}");
        assert!(
            reply["result"].get("structuredContent").is_none(),
            "{name}: no duplicate copy"
        );
        assert_eq!(reply["result"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["response"], "compact");
        assert_eq!(body["budget_applies_to"], "mcp_content_text_utf8_bytes");
        assert_eq!(
            body["requested_budget_bytes"].as_u64(),
            Some(*budget as u64)
        );
        assert!(
            body.get("packet_json").is_none(),
            "{name}: no nested escaped packet"
        );
        let packet = &body["packet"];
        assert!(packet.is_object(), "{name}: packet is an object");
        assert_eq!(packet["format"], "brain-packet/1");
        assert!(
            packet["budget_bytes"].as_u64().unwrap() < *budget as u64,
            "the envelope is paid for"
        );
        assert_eq!(text, content_text(&again[i + 1]).0, "{name}: deterministic");

        let all = items(packet);
        for item in &all {
            // REFS items are bare ref strings; every other item is labelled.
            if item.is_string() {
                continue;
            }
            assert!(item["trust"].is_string(), "{item}");
            let text = item["text"].as_str().unwrap_or_default();
            assert!(!text.contains("beta-only"), "{name}: project isolation");
            assert!(!text.contains("retracted lighthouse"), "{name}: retraction");
        }
        let memories: Vec<Value> = packet["sections"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["name"] == "MEMORIES")
            .flat_map(|s| s["items"].as_array().cloned().unwrap())
            .collect();
        for memory in &memories {
            assert!(
                memory["ref"]
                    .as_str()
                    .unwrap()
                    .starts_with("memory:canary/"),
                "{memory}"
            );
            let scope = memory["scope"].as_str().unwrap();
            assert!(
                scope == "project:canary/alpha" || scope == "space_shared",
                "{scope}"
            );
        }
        // `truncated` tells the truth about dropped items; small budgets must
        // drop some of this data and say so. (Retrieval caps upstream of the
        // packet are a separate, documented bound.)
        let omitted: u64 = packet["omitted"]
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_u64)
            .sum();
        assert_eq!(packet["truncated"], json!(omitted > 0), "{name}");
        if *budget <= 4_096 {
            assert!(omitted > 0, "{name}: low budgets drop items and admit it");
        }
        assert!(
            memories.len() >= previous_items || *budget == 1_300,
            "{name}"
        );
        if *budget != 1_300 {
            previous_items = memories.len();
        }
        if *budget >= 12_288 {
            let shared = memories
                .iter()
                .find(|m| m.get("shared").is_some())
                .unwrap_or_else(|| panic!("{name}: shared fact present"));
            assert_eq!(shared["trust"], "shared_claim_untrusted");
            assert_eq!(shared["shared"]["claimed_agent"], "claimed-hermes");
            assert_eq!(shared["shared"]["received_from_peer"], "local-host");
            assert_eq!(shared["kind"], "fact");
            let kinds: Vec<&str> = memories.iter().filter_map(|m| m["kind"].as_str()).collect();
            assert!(
                kinds.contains(&"note") && kinds.contains(&"fact"),
                "{kinds:?}"
            );
            assert!(
                memories.iter().any(|m| m["text"]
                    .as_str()
                    .unwrap()
                    .contains("\"quoted\" back\\slash\ttab\nnewline")),
                "escaped text survives exactly"
            );
        }
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn compact_option_is_validated_and_legacy_stays_default() {
    let home = isolated_home("contract");
    let replies = mcp(
        &home,
        None,
        &[
            call(
                1,
                "map_packet",
                json!({"space": "canary", "project": "alpha"}),
            ),
            call(
                2,
                "map_packet",
                json!({"space": "canary", "compact_response": "yes"}),
            ),
            call(
                3,
                "map_packet",
                json!({"space": "canary", "compact_response": true, "budget_bytes": 60}),
            ),
            call(
                4,
                "map_packet",
                json!({"space": "canary", "compact_response": true, "budget_bytes": 4_097}),
            ),
            call(
                5,
                "map_packet",
                json!({"space": "canary", "compact_response": true, "tier": "strong", "budget_bytes": 32_769}),
            ),
            call(
                6,
                "map_packet",
                json!({"space": "canary", "compact_response": false}),
            ),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/list"}),
            call(
                8,
                "map_packet",
                json!({"space": "canary", "compact_response": true, "budget_bytes": 0}),
            ),
        ],
    );
    // Legacy default, unchanged: nested packet_json string plus structuredContent.
    for i in [1, 6] {
        let (_, legacy) = content_text(&replies[i]);
        assert_eq!(legacy["status"], "ok");
        assert!(legacy["packet_json"].is_string());
        assert_eq!(
            legacy["budget_applies_to"],
            "packet_json only; this wrapper and the MCP envelope are not counted"
        );
        assert_eq!(replies[i]["result"]["structuredContent"], legacy);
    }
    let (_, bad) = content_text(&replies[2]);
    assert_eq!(
        (bad["status"].as_str(), bad["error_kind"].as_str()),
        (Some("error"), Some("invalid_input"))
    );
    assert!(
        bad["message"]
            .as_str()
            .unwrap()
            .contains("compact_response must be a boolean")
    );
    let (_, small) = content_text(&replies[3]);
    assert_eq!(small["status"], "error");
    assert!(
        small["message"]
            .as_str()
            .unwrap()
            .contains("cannot hold a compact response"),
        "{small}"
    );
    for i in [4, 5, 8] {
        let (_, over) = content_text(&replies[i]);
        assert_eq!(
            over["error_kind"], "invalid_input",
            "no silent widening: {over}"
        );
        assert_eq!(replies[i]["result"]["isError"], true);
    }
    let tools = replies[7]["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 27, "no new tool");
    let packet_tool = tools.iter().find(|t| t["name"] == "map_packet").unwrap();
    assert_eq!(
        packet_tool.pointer("/inputSchema/properties/compact_response/type"),
        Some(&json!("boolean"))
    );
    assert!(
        packet_tool["description"]
            .as_str()
            .unwrap()
            .contains("WHOLE UTF-8 content text")
    );
    let _ = fs::remove_dir_all(&home);
}
