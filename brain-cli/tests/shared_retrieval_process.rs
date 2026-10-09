//! Shared facts in map packets through the real `brain` binary on a synthetic,
//! isolated HOME: read only from the host-configured share state directory,
//! default deny without it, space opt-in, project isolation before limits,
//! typed kinds, claimed vs recorded provenance, retraction and byte budgets.

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
        "brain-shared-retrieval-{tag}-{}-{nonce}",
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
            "probe-shared",
        ]);
    command
}

fn share(home: &Path, state: &Path, args: &[&str]) -> Value {
    let output = brain(home)
        .args(["net", "share"])
        .args(&args[..1])
        .args(["--state-dir", state.to_str().expect("utf8")])
        .args(&args[1..])
        .output()
        .expect("run share");
    let text = String::from_utf8_lossy(&output.stdout);
    let document: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|_| panic!("{text} {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(document["status"], "ok", "{document}");
    document
}

/// One stdio MCP conversation against `brain serve`, optionally with the
/// host-configured share state directory.
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
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})
        )
        .unwrap();
        for call in calls {
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

fn packet_call(id: i64, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"map_packet","arguments":arguments}})
}

fn wrapper(reply: &Value) -> Value {
    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn packet_of(reply: &Value) -> Value {
    let wrapper = wrapper(reply);
    assert_eq!(wrapper["status"], "ok", "{wrapper}");
    let json = wrapper["packet_json"].as_str().unwrap();
    assert_eq!(
        wrapper["packet_bytes"].as_u64().unwrap() as usize,
        json.len()
    );
    serde_json::from_str(json).unwrap()
}

fn shared_items(packet: &Value) -> Vec<Value> {
    packet["sections"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["items"].as_array().cloned().unwrap_or_default())
        .filter(|item| item.get("shared").is_some())
        .collect()
}

fn alpha_args(extra: Value) -> Value {
    let mut args = json!({"space": "canary", "project": "alpha", "memory_query": "lighthouse"});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    args
}

#[test]
fn shared_facts_reach_packets_only_from_the_host_configured_store() {
    let home = isolated_home("wired");
    let state = home.join("net-state");
    fs::create_dir_all(&state).unwrap();
    share(&home, &state, &["enable", "--space", "canary"]);
    share(
        &home,
        &state,
        &[
            "publish",
            "--space",
            "canary",
            "--fact-id",
            "A1",
            "--payload",
            r#"{"project":"alpha","note":"lighthouse"}"#,
            "--claim",
            "alpha lighthouse fact",
            "--claimed-agent",
            "claimed-codex",
        ],
    );
    share(
        &home,
        &state,
        &[
            "publish",
            "--space",
            "canary",
            "--fact-id",
            "L1",
            "--kind",
            "link",
            "--payload",
            r#"{"project":"alpha","from":"task:canary/1","to":"task:canary/2"}"#,
            "--claim",
            "lighthouse depends",
        ],
    );
    // Many higher-sequence matches of another project must not crowd alpha out.
    for i in 0..25 {
        share(
            &home,
            &state,
            &[
                "publish",
                "--space",
                "canary",
                "--fact-id",
                &format!("B{i}"),
                "--payload",
                r#"{"project":"beta","note":"lighthouse"}"#,
                "--claim",
                "beta lighthouse fact",
            ],
        );
    }

    // Default deny: without the startup flag nothing shared is read.
    let replies = mcp(&home, None, &[packet_call(2, alpha_args(json!({})))]);
    assert!(shared_items(&packet_of(&replies[1])).is_empty());

    // An MCP request can never name the store.
    let replies = mcp(
        &home,
        None,
        &[packet_call(
            2,
            alpha_args(json!({"share_state_dir": state.to_str().unwrap()})),
        )],
    );
    assert!(
        replies[1].to_string().contains("unknown argument"),
        "{}",
        replies[1]
    );

    let replies = mcp(
        &home,
        Some(&state),
        &[
            packet_call(2, alpha_args(json!({"tier": "strong"}))),
            packet_call(
                3,
                json!({"space": "canary", "project": "beta", "memory_query": "lighthouse", "tier": "strong"}),
            ),
            packet_call(
                4,
                json!({"space": "elsewhere", "project": "alpha", "memory_query": "lighthouse", "tier": "strong"}),
            ),
            packet_call(5, alpha_args(json!({"tier": "weak", "budget_bytes": 1024}))),
        ],
    );
    let alpha = shared_items(&packet_of(&replies[1]));
    let mut kinds: Vec<&str> = alpha.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        vec!["fact", "link"],
        "typed, never decision/gotcha: {alpha:?}"
    );
    for item in &alpha {
        assert_eq!(item["trust"], "shared_claim_untrusted");
        assert_eq!(item["scope"], "project:canary/alpha");
        assert_eq!(
            item["shared"]["received_from_peer"], "local-host",
            "recorded by this host"
        );
        assert!(
            item["ref"]
                .as_str()
                .unwrap()
                .starts_with("memory:canary/shared-")
        );
    }
    let fact = alpha.iter().find(|i| i["kind"] == "fact").unwrap();
    assert_eq!(
        fact["shared"]["claimed_agent"], "claimed-codex",
        "a claim, apart from the peer"
    );
    assert_eq!(fact["shared"]["fact_id"], "A1");
    let link = alpha.iter().find(|i| i["kind"] == "link").unwrap();
    assert!(
        link["text"].as_str().unwrap().contains("task:canary/2"),
        "links keep their refs"
    );

    let beta = shared_items(&packet_of(&replies[2]));
    assert!(!beta.is_empty() && beta.iter().all(|i| i["scope"] == "project:canary/beta"));
    assert!(
        shared_items(&packet_of(&replies[3])).is_empty(),
        "a space the host never enabled reads nothing"
    );
    let small = wrapper(&replies[4]);
    assert!(small["packet_bytes"].as_u64().unwrap() <= 1024, "{small}");

    // Retraction: the fact leaves retrieval.
    share(
        &home,
        &state,
        &["retract", "--space", "canary", "--fact-id", "A1"],
    );
    let replies = mcp(
        &home,
        Some(&state),
        &[packet_call(2, alpha_args(json!({"tier": "strong"})))],
    );
    let after = shared_items(&packet_of(&replies[1]));
    assert!(
        after.iter().all(|i| i["shared"]["fact_id"] != "A1"),
        "{after:?}"
    );
    assert_eq!(after.len(), 1);

    // Disabling the space on this host stops reading it.
    share(&home, &state, &["disable", "--space", "canary"]);
    let replies = mcp(
        &home,
        Some(&state),
        &[packet_call(2, alpha_args(json!({})))],
    );
    assert!(shared_items(&packet_of(&replies[1])).is_empty());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_relative_or_missing_share_state_dir_is_safe() {
    let home = isolated_home("config");
    let output = brain(&home)
        .args(["--share-state-dir", "relative/dir", "map", "status"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("absolute"));
    // Configured but never created: nothing is read and nothing is created.
    let missing = home.join("no-such-state");
    let replies = mcp(
        &home,
        Some(&missing),
        &[packet_call(2, alpha_args(json!({})))],
    );
    assert!(shared_items(&packet_of(&replies[1])).is_empty());
    assert!(!missing.exists(), "a read never creates the share store");
    let _ = fs::remove_dir_all(&home);
}
