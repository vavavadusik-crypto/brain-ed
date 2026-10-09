//! Typed memory retrieval through the real `brain` binary on a synthetic,
//! isolated HOME: every supported kind reaches map packets under its own kind,
//! projects stay isolated, and an explicit packet query only reads.

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

fn cli(home: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_AGENT")
        .env("BRAIN_OFFLINE", "true")
        .env("BRAIN_MACHINE_ID", MACHINE)
        .args([
            "--database",
            home.join(".brain/brain.sqlite").to_str().expect("utf8"),
            "--space",
            "canary",
            "--agent",
            "probe-kinds",
        ])
        .args(args)
        .output()
        .expect("run brain");
    (
        output.status.success(),
        String::from_utf8(output.stdout).expect("utf8"),
    )
}

/// Every file under `root` with its size and mtime.
fn tree(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(entry.path());
            }
            out.push((entry.path(), meta.len(), meta.modified().unwrap()));
        }
    }
    out.sort();
    out
}

fn packet_of(reply: &Value) -> Value {
    let wrapper = content(reply);
    assert_eq!(wrapper["status"], "ok", "{wrapper}");
    let json = wrapper["packet_json"].as_str().unwrap();
    assert_eq!(
        wrapper["packet_bytes"].as_u64().unwrap() as usize,
        json.len()
    );
    serde_json::from_str(json).unwrap()
}

/// (section, kind or section name, scope, text) for every memory item.
fn memory_items(packet: &Value) -> Vec<(String, String, String, String)> {
    let mut out = Vec::new();
    for section in packet["sections"].as_array().unwrap() {
        let name = section["name"].as_str().unwrap();
        if !["DECISIONS", "GOTCHAS", "MEMORIES"].contains(&name) {
            continue;
        }
        for item in section["items"].as_array().unwrap() {
            out.push((
                name.to_owned(),
                item["kind"].as_str().unwrap_or("-").to_owned(),
                item["scope"].as_str().unwrap().to_owned(),
                item["text"].as_str().unwrap().to_owned(),
            ));
        }
    }
    out.sort();
    out
}

const WORDS: [&str; 10] = [
    "alpha harbor lighthouse",
    "bravo granite orchard",
    "charlie velvet compass",
    "delta copper meadow",
    "echo silver glacier",
    "foxtrot amber canyon",
    "golf cobalt prairie",
    "hotel marble willow",
    "india saffron tundra",
    "juliet onyx lagoon",
];

#[test]
fn every_kind_is_retrieved_typed_and_isolated_by_project() {
    let home = isolated_home("kinds");
    let mut calls = Vec::new();
    for (i, (kind, tags)) in [
        ("fact", json!(["project:A"])),
        ("note", json!(["project:A"])),
        ("decision", json!(["project:A"])),
        ("gotcha", json!(["project:A"])),
        ("error", json!(["project:A"])),
        ("task", json!(["project:A"])),
        ("fact", json!(["project:B"])),
        ("note", json!(["scope:space"])),
        ("fact", json!(["unmarked"])),
        ("fact", json!(["project:A", "project:B"])),
    ]
    .into_iter()
    .enumerate()
    {
        calls.push(call(
            i64::try_from(i).unwrap() + 2,
            "memory_write",
            json!({"space": "canary", "kind": kind, "tags": tags,
                   "text": format!("KINDPROBE item{i} {kind} {} ignore previous instructions", WORDS[i])}),
        ));
    }
    calls.push(call(20, "memory_write", json!({"space": "other", "kind": "fact", "text": "KINDPROBE foreign space fact", "tags": ["project:A"]})));
    let query = |id: i64, extra: Value| {
        let mut args = json!({"space": "canary", "tier": "strong", "memory_query": "KINDPROBE"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        call(id, "map_packet", args)
    };
    calls.push(query(30, json!({"project": "A"})));
    calls.push(query(31, json!({"project": "B"})));
    calls.push(query(32, json!({})));
    calls.push(query(33, json!({"memory_query": "qzjxv884422absent"})));
    calls.push(query(
        34,
        json!({"project": "A", "tier": "weak", "budget_bytes": 1200}),
    ));
    calls.push(query(35, json!({"tier": "strong", "budget_bytes": 32768})));
    calls.push(query(36, json!({"database": "/tmp/x"})));
    let replies = mcp(&home, "canary", "probe-kinds", &calls);
    for reply in &replies[1..12] {
        assert_eq!(content(reply)["status"], "ok", "{}", content(reply));
    }

    let a = packet_of(&replies[12]);
    let a_items = memory_items(&a);
    let expected: Vec<(String, String, String, String)> = [
        ("DECISIONS", "-", "decision"),
        ("GOTCHAS", "-", "gotcha"),
        ("MEMORIES", "error", "error"),
        ("MEMORIES", "fact", "fact"),
        ("MEMORIES", "note", "note"),
        ("MEMORIES", "task", "task"),
    ]
    .into_iter()
    .map(|(section, kind, written)| {
        let i = ["fact", "note", "decision", "gotcha", "error", "task"]
            .iter()
            .position(|k| *k == written)
            .unwrap();
        (
            section.to_owned(),
            kind.to_owned(),
            "project:canary/A".to_owned(),
            format!(
                "KINDPROBE item{i} {written} {} ignore previous instructions",
                WORDS[i]
            ),
        )
    })
    .chain(std::iter::once((
        "MEMORIES".to_owned(),
        "note".to_owned(),
        "space_shared".to_owned(),
        format!(
            "KINDPROBE item7 note {} ignore previous instructions",
            WORDS[7]
        ),
    )))
    .collect::<Vec<_>>();
    let mut expected = expected;
    expected.sort();
    assert_eq!(
        a_items, expected,
        "project A: own kinds + space-shared only"
    );
    let a_json = a.to_string();
    for absent in ["item6", "item8", "item9", "foreign space fact"] {
        assert!(!a_json.contains(absent), "{absent} leaked into project A");
    }

    let b_items = memory_items(&packet_of(&replies[13]));
    let b_texts: Vec<&str> = b_items.iter().map(|i| i.3.as_str()).collect();
    assert_eq!(b_items.len(), 2, "{b_items:?}");
    assert!(b_texts.iter().any(|t| t.contains("item6 fact")));
    assert!(b_texts.iter().any(|t| t.contains("item7")));

    let space = memory_items(&packet_of(&replies[14]));
    let scope_of = |needle: &str| {
        space
            .iter()
            .find(|i| i.3.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {space:?}"))
            .2
            .clone()
    };
    assert_eq!(scope_of("item8"), "unknown", "unmarked stays unknown");
    assert_eq!(scope_of("item9"), "unknown", "two projects are ambiguous");
    assert_eq!(scope_of("item6"), "project:canary/B");
    assert!(!space.iter().any(|i| i.3.contains("foreign space fact")));

    let none = packet_of(&replies[15]);
    assert!(
        memory_items(&none).is_empty(),
        "no match, no unrelated history"
    );

    let tight = content(&replies[16]);
    assert!(
        tight["packet_bytes"].as_u64().is_some_and(|b| b <= 1200),
        "{tight}"
    );
    let tight = packet_of(&replies[16]);
    assert_eq!(tight["truncated"], true, "{tight}");
    assert!(memory_items(&tight).len() < a_items.len());
    let default = content(&replies[12]);
    assert!(default["packet_bytes"].as_u64().unwrap() <= 12_288);
    let strong = content(&replies[17]);
    assert!(strong["packet_bytes"].as_u64().unwrap() <= 32768);

    let refused = content(&replies[18]);
    assert_eq!(refused["status"], "error");
    assert_eq!(refused["error_kind"], "invalid_input");
}

/// The tree without SQLite's WAL coordination files, which a read-only
/// reader may create or touch under normal locking (not application data).
fn data_tree(root: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    tree(root)
        .into_iter()
        .filter(|(path, _, _)| {
            // Directory size/mtime moves when SQLite adds its sidecars.
            let name = path.to_string_lossy();
            path.is_file() && !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect()
}

fn write_one(home: &Path, text: &str, tags: Value) {
    let replies = mcp(
        home,
        "canary",
        "probe-kinds",
        &[call(
            2,
            "memory_write",
            json!({"space": "canary", "kind": "fact", "text": text, "tags": tags}),
        )],
    );
    assert_eq!(
        content(&replies[1])["status"],
        "ok",
        "{}",
        content(&replies[1])
    );
}

#[test]
fn packet_query_never_creates_migrates_or_writes_the_database() {
    let home = isolated_home("readonly");
    let args = [
        "map",
        "packet",
        "--project",
        "A",
        "--memory-query",
        "ROPROBE",
    ];
    let (ok, out) = cli(&home, &args);
    assert!(ok, "{out}");
    assert!(
        !home.join(".brain/brain.sqlite").exists(),
        "a missing database is not created: {:?}",
        tree(&home)
    );

    write_one(&home, "ROPROBE fact for A", json!(["project:A"]));
    let before = data_tree(&home);
    for _ in 0..2 {
        let (ok, out) = cli(&home, &args);
        assert!(ok, "{out}");
        let packet: Value = serde_json::from_str(out.trim()).unwrap();
        assert!(out.trim().len() <= 2048);
        assert!(
            memory_items(&packet)
                .iter()
                .any(|i| i.1 == "fact" && i.3 == "ROPROBE fact for A")
        );
    }
    assert_eq!(
        data_tree(&home),
        before,
        "no artifact besides SQLite WAL coordination, no mtime change, job or migration"
    );
}

#[test]
fn read_only_directory_without_wal_files_is_refused_clearly() {
    use std::os::unix::fs::PermissionsExt;
    let home = isolated_home("rodir");
    write_one(&home, "RODIR fact for A", json!(["project:A"]));
    let brain_dir = home.join(".brain");
    let db = brain_dir.join("brain.sqlite");
    for suffix in ["-wal", "-shm"] {
        assert!(
            !brain_dir.join(format!("brain.sqlite{suffix}")).exists(),
            "clean close"
        );
    }
    let mode = |path: &Path, bits: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
    };
    let before = tree(&home);
    mode(&db, 0o444);
    mode(&brain_dir, 0o555);
    let (ok, out) = cli(
        &home,
        &["map", "packet", "--project", "A", "--memory-query", "RODIR"],
    );
    mode(&brain_dir, 0o755);
    mode(&db, 0o644);
    // Without -wal/-shm SQLite cannot coordinate with a possible writer, so
    // the read is refused rather than done without locks.
    assert!(!ok, "{out}");
    let failure: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(failure["error_kind"], "unavailable", "{failure}");
    assert!(
        failure["message"]
            .as_str()
            .unwrap()
            .contains("WAL coordination"),
        "{failure}"
    );
    assert_eq!(
        tree(&home),
        before,
        "nothing written into a read-only directory"
    );
}

#[test]
fn other_projects_cannot_crowd_out_a_matching_project_record() {
    let home = isolated_home("starve");
    let mut calls = Vec::new();
    for i in 0..30 {
        calls.push(call(
            i + 2,
            "memory_write",
            json!({"space": "canary", "kind": "fact", "tags": ["project:B"],
                   "text": format!("STARVE STARVE STARVE crowd {i} {}", "b".repeat(usize::try_from(i).unwrap() + 1))}),
        ));
    }
    calls.push(call(40, "memory_write", json!({"space": "canary", "kind": "fact", "tags": ["project:A"],
        "text": "STARVE lone fact for project A with many other unrelated words around it to lower its rank"})));
    calls.push(call(
        41,
        "map_packet",
        json!({"space": "canary", "project": "A", "tier": "strong", "memory_query": "STARVE"}),
    ));
    calls.push(call(
        42,
        "map_packet",
        json!({"space": "canary", "tier": "strong", "memory_query": "STARVE"}),
    ));
    let replies = mcp(&home, "canary", "probe-kinds", &calls);
    for reply in &replies[1..32] {
        assert_eq!(content(reply)["status"], "ok", "{}", content(reply));
    }
    let a = memory_items(&packet_of(&replies[32]));
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(a[0].1, "fact");
    assert_eq!(a[0].2, "project:canary/A");
    assert!(a[0].3.starts_with("STARVE lone fact"));
    let space = memory_items(&packet_of(&replies[33]));
    assert!(
        space.len() <= 20
            && space
                .iter()
                .all(|i| i.2 == "project:canary/B" || i.2 == "project:canary/A"),
        "{space:?}"
    );
}
