//! Synthetic, process-level CLI/MCP parity tests. No user archive or model I/O.
#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = Self(test_support::temp_path("brain-ask-archive"));
        fs::create_dir_all(fixture.0.join(".brain")).unwrap();
        fixture.settings(2400, None);
        fixture.cli(&["remember", "8803 core decision Alpha", "--tag", "selected"]);
        fixture
    }

    fn settings(&self, budget: usize, include: Option<bool>) {
        let mut settings = json!({"version":1,"profile":"balanced","embedding_mode":"off","overrides":{"budget_tokens":budget}});
        if let Some(include) = include {
            settings["include_archive"] = json!(include);
        }
        fs::write(self.0.join(".brain/settings.json"), settings.to_string()).unwrap();
    }

    /// Compare searches from identical state: reads update access/recency scores.
    fn fork(&self) -> Self {
        fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
            fs::create_dir_all(to).unwrap();
            fs::set_permissions(to, fs::metadata(from).unwrap().permissions()).unwrap();
            for entry in fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), target).unwrap();
                }
            }
        }
        let fork = Self(test_support::temp_path("brain-ask-archive-fork"));
        copy_tree(&self.0, &fork.0);
        fork
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brain"));
        command
            .env("HOME", &self.0)
            .env_remove("BRAIN_DATABASE")
            .env_remove("BRAIN_MODELS_DIR")
            .args([
                "--offline",
                "--space",
                "archive-test",
                "--agent",
                "archive-test",
            ]);
        command
    }

    fn cli(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn mcp_request(&self, request: Value) -> Value {
        let mut child = self
            .command()
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn mcp(&self, args: Value) -> Value {
        let reply = self.mcp_request(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_search","arguments":args}}));
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        reply["result"]["structuredContent"].clone()
    }

    fn archive(&self, texts: &[&str]) -> PathBuf {
        let directory = self.0.join(".brain/archive");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("sessions.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("
            CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:2');
            CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT, path_sha256 TEXT);
            INSERT INTO sources VALUES (1, 'codex', 'synthetic');
            CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER, session_id TEXT, role TEXT,
                locator TEXT, source_offset INTEGER, ts TEXT, content TEXT, content_sha256 TEXT,
                evidence_sha256 TEXT, record_sha256 TEXT, imported_at TEXT);
            CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id');
            CREATE TABLE quarantine(entry_id INTEGER, reason TEXT, released_at TEXT);
        ").unwrap();
        for (index, text) in texts.iter().enumerate() {
            let id = index as i64 + 1;
            db.execute("INSERT INTO entries VALUES (?1,1,'session-a','assistant','synthetic',0,'2026-01-01T00:00:00Z',?2,'synthetic','synthetic','synthetic','2026-01-01T00:00:01Z')", params![id, text]).unwrap();
            db.execute(
                "INSERT INTO entries_fts(rowid,content) VALUES (?1,?2)",
                params![id, text],
            )
            .unwrap();
        }
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// Same schema, but each entry's `ts` is a numeric (Hermes-style)
    /// timestamp the given number of seconds before now.
    fn archive_aged(&self, entries: &[(&str, i64)]) -> PathBuf {
        let directory = self.0.join(".brain/archive");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("sessions.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("
            CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:2');
            CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT, path_sha256 TEXT);
            INSERT INTO sources VALUES (1, 'codex', 'synthetic');
            CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER, session_id TEXT, role TEXT,
                locator TEXT, source_offset INTEGER, ts TEXT, content TEXT, content_sha256 TEXT,
                evidence_sha256 TEXT, record_sha256 TEXT, imported_at TEXT);
            CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id');
            CREATE TABLE quarantine(entry_id INTEGER, reason TEXT, released_at TEXT);
        ").unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (index, (text, age_seconds)) in entries.iter().enumerate() {
            let id = index as i64 + 1;
            db.execute(
                "INSERT INTO entries VALUES (?1,1,'session-a','assistant','synthetic',0,?2,?3,'synthetic','synthetic','synthetic','2026-01-01T00:00:01Z')",
                params![id, (now - age_seconds).to_string(), text],
            )
            .unwrap();
            db.execute(
                "INSERT INTO entries_fts(rowid,content) VALUES (?1,?2)",
                params![id, text],
            )
            .unwrap();
        }
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// Same schema, but each entry carries the given ISO-8601 `ts`.
    fn archive_dated(&self, entries: &[(&str, &str)]) -> PathBuf {
        let directory = self.0.join(".brain/archive");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("sessions.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("
            CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:2');
            CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT, path TEXT, path_sha256 TEXT);
            INSERT INTO sources VALUES (1, 'codex', '/synthetic/state.db', 'synthetic');
            CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER, session_id TEXT, role TEXT,
                locator TEXT, source_offset INTEGER, ts TEXT, content TEXT, content_sha256 TEXT,
                evidence_sha256 TEXT, record_sha256 TEXT, imported_at TEXT);
            CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id');
            CREATE TABLE quarantine(entry_id INTEGER, reason TEXT, released_at TEXT);
        ").unwrap();
        for (index, (ts, text)) in entries.iter().enumerate() {
            let id = index as i64 + 1;
            db.execute(
                "INSERT INTO entries VALUES (?1,1,'session-a','assistant','synthetic',0,?2,?3,'synthetic','synthetic','synthetic','2026-01-01T00:00:01Z')",
                params![id, ts, text],
            )
            .unwrap();
            db.execute(
                "INSERT INTO entries_fts(rowid,content) VALUES (?1,?2)",
                params![id, text],
            )
            .unwrap();
        }
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    /// Same schema, but each entry carries the given (role, text); `ts` stays
    /// in the far past so the ask-side age filter keeps everything.
    fn archive_roles(&self, entries: &[(&str, &str)]) -> PathBuf {
        let directory = self.0.join(".brain/archive");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("sessions.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("
            CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:2');
            CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT, path TEXT, path_sha256 TEXT);
            INSERT INTO sources VALUES (1, 'codex', '/synthetic/state.db', 'synthetic');
            CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER, session_id TEXT, role TEXT,
                locator TEXT, source_offset INTEGER, ts TEXT, content TEXT, content_sha256 TEXT,
                evidence_sha256 TEXT, record_sha256 TEXT, imported_at TEXT);
            CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id');
            CREATE TABLE quarantine(entry_id INTEGER, reason TEXT, released_at TEXT);
        ").unwrap();
        for (index, (role, text)) in entries.iter().enumerate() {
            let id = index as i64 + 1;
            db.execute("INSERT INTO entries VALUES (?1,1,'session-a',?2,'synthetic',0,'2026-01-01T00:00:00Z',?3,'synthetic','synthetic','synthetic','2026-01-01T00:00:01Z')", params![id, role, text]).unwrap();
            db.execute(
                "INSERT INTO entries_fts(rowid,content) VALUES (?1,?2)",
                params![id, text],
            )
            .unwrap();
        }
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn results(value: &Value) -> &[Value] {
    value["results"].as_array().unwrap()
}
fn conversations(value: &Value) -> Vec<Value> {
    results(value)
        .iter()
        .filter(|hit| hit["kind"] == "conversation")
        .cloned()
        .collect()
}
fn has_archive_warning(value: &Value) -> bool {
    value["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str().unwrap().contains("archive"))
}
fn assert_budget(value: &Value) {
    let budget = value["budget_tokens"].as_u64().unwrap();
    let used: u64 = results(value)
        .iter()
        .map(|hit| hit["token_len"].as_u64().unwrap())
        .sum();
    let archive_used: u64 = conversations(value)
        .iter()
        .map(|hit| hit["token_len"].as_u64().unwrap())
        .sum();
    assert_eq!(value["used_tokens"], used);
    assert!(used <= budget, "{value}");
    assert!(archive_used <= budget / 3, "{value}");
    assert_eq!(value["archive_hits"], conversations(value).len());
}

#[test]
fn knowledge_fusion_keeps_core_first_and_preserves_archive_provenance() {
    let f = Fixture::new();
    f.cli(&[
        "remember",
        "8803 another durable memory about Beta unrelated",
        "--kind",
        "decision",
    ]);
    f.archive(&[
        "8803 archive short",
        "8803 archive second much longer description",
    ]);
    let core = f.fork().cli(&["ask", "8803", "--no-archive"]);
    let archive = f.cli(&["archive", "search", "8803"]);
    let answer = f.fork().cli(&["ask", "8803"]);
    let hits = results(&answer);
    assert_eq!(hits.len(), 4, "{answer}");
    for rank in 0..2 {
        assert_eq!(hits[rank]["id"], core["results"][rank]["id"]);
        let conversation = &hits[2 + rank];
        assert_eq!(conversation["id"], archive["results"][rank]["id"]);
        assert!((hits[rank]["score"].as_f64().unwrap() - 1.0 / (61 + rank) as f64).abs() < 1e-12);
        assert!((conversation["score"].as_f64().unwrap() - 1.0 / (63 + rank) as f64).abs() < 1e-12);
        assert_eq!(conversation["kind"], "conversation");
        assert_eq!(conversation["client"], "codex");
        assert_eq!(conversation["role"], "assistant");
        assert_eq!(conversation["session_id"], "session-a");
        assert_eq!(conversation["timestamp"], "2026-01-01T00:00:00Z");
        assert_eq!(conversation["untrusted"], true);
        assert_eq!(
            conversation["source"],
            format!("archive://codex/session-a/{}", conversation["id"])
        );
        assert!(conversation.get("space").is_none());
    }
    assert_eq!(answer["archive_included"], true);
    assert_budget(&answer);
}

#[test]
fn knowledge_top_five_survives_archive_on_cli_and_limited_mcp() {
    let f = Fixture::new();
    for text in [
        "8803 durable storage with SQLite",
        "8803 runtime modes and workers",
        "8803 cache model embeddings",
        "8803 viewer graph visualization",
        "8803 configuration and quality gates",
    ] {
        f.cli(&["remember", text]);
    }
    f.archive(&["8803 conversation one", "8803 conversation two"]);
    let ids = |answer: &Value| {
        results(answer)
            .iter()
            .take(5)
            .map(|hit| hit["id"].clone())
            .collect::<Vec<_>>()
    };
    let core = f.fork().cli(&["ask", "8803", "--no-archive"]);
    assert!(results(&core).len() >= 5);
    let with = f.fork().cli(&["ask", "8803"]);
    assert_eq!(ids(&with), ids(&core));
    assert_eq!(with["archive_included"], true);
    assert!(with["archive_hits"].as_u64().unwrap() > 0);
    let core = f
        .fork()
        .mcp(json!({"query":"8803", "include_archive":false}));
    let with = f.fork().mcp(json!({"query":"8803"}));
    assert_eq!(ids(&with), ids(&core));
    assert_eq!(with["archive_hits"], 0);
    assert_budget(&with);
}

#[test]
fn explicit_conversation_query_keeps_archive_in_top_five_on_cli_and_mcp() {
    let f = Fixture::new();
    for suffix in [
        "SQLite storage",
        "worker runtime",
        "model cache",
        "graph viewer",
        "quality gates",
    ] {
        f.cli(&["remember", &format!("8803 chat {suffix}")]);
    }
    f.archive(&["8803 chat first discussion", "8803 chat second discussion"]);
    let cli = f.cli(&["ask", "8803 chat"]);
    let mcp = f.mcp(json!({"query":"8803 chat"}));
    for answer in [&cli, &mcp] {
        let hits = results(answer);
        assert!(hits.len() >= 5);
        assert_eq!(hits[0]["kind"], "fact");
        assert_eq!(hits[1]["kind"], "conversation");
        assert_eq!(hits[2]["kind"], "fact");
        assert_eq!(hits[3]["kind"], "conversation");
        assert_budget(answer);
    }
    assert_eq!(conversations(&cli), conversations(&mcp));
}

#[test]
fn budgets_bound_unicode_excerpts_and_leave_room_for_core() {
    let f = Fixture::new();
    let text = format!("8803 {}", "разговор 🦀 灯塔 ".repeat(200));
    f.archive(&[&text, &text, &text]);
    for budget in [0, 1, 3, 100, 300, 600, 1200, 2400] {
        let answer = f.cli(&["ask", "8803", "--budget-tokens", &budget.to_string()]);
        assert_eq!(answer["budget_tokens"], budget);
        assert_budget(&answer);
        for hit in conversations(&answer) {
            let excerpt = hit["excerpt"].as_str().unwrap();
            assert!(excerpt.chars().count() <= 301);
            assert_eq!(hit["token_len"], excerpt.len() + 1);
        }
        if budget >= 300 {
            assert!(results(&answer).iter().any(|hit| hit["kind"] == "fact"));
            assert!(!conversations(&answer).is_empty());
        }
    }
}

#[test]
fn no_archive_and_recall_do_not_search_archive_and_settings_persist() {
    let f = Fixture::new();
    f.archive(&["8803 archive answer"]);
    for args in [
        &["ask", "8803", "--no-archive"][..],
        &["recall", "8803"][..],
    ] {
        let answer = f.cli(args);
        assert_eq!(answer["archive_included"], false);
        assert_eq!(answer["archive_hits"], 0);
        assert_eq!(results(&answer).len(), 1);
    }
    let saved = f.cli(&["config", "set", "include_archive", "false"]);
    assert_eq!(saved["include_archive"], false);
    assert_eq!(f.cli(&["config", "show"])["include_archive"], false);
    let disabled = f.cli(&["ask", "8803"]);
    assert_eq!(disabled["archive_included"], false);
    assert!(has_archive_warning(&disabled));
    f.cli(&["config", "set", "include_archive", "true"]);
    assert_eq!(f.cli(&["ask", "8803"])["archive_hits"], 1);
    let invalid = f
        .command()
        .args(["config", "set", "include_archive", "sometimes"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(f.cli(&["config", "show"])["include_archive"], true);
}

#[test]
fn missing_locked_and_disabled_archive_degrade_to_core() {
    let f = Fixture::new();
    let missing = f.cli(&["ask", "8803"]);
    assert_eq!(missing["archive_included"], false);
    assert_eq!(results(&missing).len(), 1);
    assert!(has_archive_warning(&missing));
    let missing_mcp = f.mcp(json!({"query":"8803"}));
    assert_eq!(missing_mcp["archive_included"], false);
    assert_eq!(results(&missing_mcp).len(), 1);
    assert!(has_archive_warning(&missing_mcp));
    assert!(!f.0.join(".brain/archive").exists());
    let path = f.archive(&["8803 archived"]);
    let db = Connection::open(path).unwrap();
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let locked = f.cli(&["ask", "8803"]);
    assert_eq!(locked["archive_hits"], 0);
    assert_eq!(results(&locked).len(), 1);
    assert!(has_archive_warning(&locked));
    db.execute_batch("ROLLBACK").unwrap();
    fs::write(
        f.0.join(".brain/archive/collector.json"),
        r#"{"enabled":false}"#,
    )
    .unwrap();
    let paused = f.cli(&["ask", "8803"]);
    assert_eq!(paused["archive_included"], false);
    assert_eq!(results(&paused).len(), 1);
    assert!(has_archive_warning(&paused));
}

#[test]
fn quarantine_is_excluded_in_cli_and_mcp() {
    let f = Fixture::new();
    let path = f.archive(&["8803 visible evidence", "8803 quarantined secret"]);
    let db = Connection::open(path).unwrap();
    db.execute("INSERT INTO quarantine VALUES (2,'synthetic',NULL)", [])
        .unwrap();
    for answer in [f.cli(&["ask", "8803"]), f.mcp(json!({"query":"8803"}))] {
        assert_eq!(answer["archive_hits"], 1, "{answer}");
        assert_eq!(conversations(&answer)[0]["id"], 1);
        assert!(!answer.to_string().contains("quarantined secret"));
    }
}

#[test]
fn mcp_matches_cli_and_honors_settings_overrides_filters_and_schema() {
    let f = Fixture::new();
    f.archive(&["8803 archived decision", "8803 archived second"]);
    let cli = f.cli(&["ask", "8803"]);
    let mcp = f.mcp(json!({"query":"8803"}));
    assert_eq!(conversations(&cli), conversations(&mcp));
    for key in [
        "archive_hits",
        "archive_included",
        "used_tokens",
        "budget_tokens",
    ] {
        assert_eq!(cli[key], mcp[key]);
    }
    assert_budget(&mcp);
    for budget in [100, 300, 600] {
        f.settings(budget, None);
        let cli = f.cli(&["ask", "8803"]);
        let mcp = f.mcp(json!({"query":"8803"}));
        assert_eq!(conversations(&cli), conversations(&mcp));
        assert_budget(&mcp);
    }
    f.settings(2400, None);
    let limited = f.mcp(json!({"query":"8803", "limit":1}));
    assert_eq!(results(&limited).len(), 1);
    assert_budget(&limited);
    let excluded = f.mcp(json!({"query":"8803", "include_archive":false}));
    assert_eq!(excluded["archive_hits"], 0);
    f.cli(&["config", "set", "include_archive", "false"]);
    assert_eq!(f.mcp(json!({"query":"8803"}))["archive_included"], false);
    assert_eq!(
        f.mcp(json!({"query":"8803", "include_archive":true}))["archive_hits"],
        2
    );
    let tagged = f.mcp(json!({"query":"8803", "tags":["selected"], "include_archive":true}));
    assert_eq!(tagged["archive_hits"], 0);
    assert_eq!(results(&tagged).len(), 1);
    let recent = f.mcp(json!({"query":"8803", "since":"2026-02-01", "include_archive":true}));
    assert_eq!(recent["archive_hits"], 0);
    let old = f.mcp(json!({"query":"8803", "since":"2025-01-01", "include_archive":true}));
    assert_eq!(old["archive_hits"], 2);
    let reply = f.mcp_request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let definition = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "memory_search")
        .unwrap();
    assert_eq!(
        definition["inputSchema"]["properties"]["include_archive"]["type"],
        "boolean"
    );
    let bad = f.mcp_request(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"8803","include_archive":"false"}}}));
    assert_eq!(bad["result"]["isError"], true);
}

#[test]
fn archive_only_questions_and_uri_components_are_supported() {
    let f = Fixture::new();
    let path = f.archive(&["Ollama Cloud ADR-004 8803 answer"]);
    let db = Connection::open(path).unwrap();
    db.execute("UPDATE entries SET session_id = 'a/b ?#%'", [])
        .unwrap();
    for query in ["Ollama Cloud", "ADR-004"] {
        let answer = f.cli(&["ask", query]);
        assert_eq!(answer["archive_hits"], 1, "{answer}");
        assert_eq!(
            conversations(&answer)[0]["source"],
            "archive://codex/a%2Fb%20%3F%23%25/1"
        );
        assert_budget(&answer);
    }
}

#[test]
fn young_and_self_quoting_archive_hits_are_filtered_and_counted() {
    let f = Fixture::new();
    f.archive_aged(&[
        ("8803 chatter from the session happening right now", 5 * 60),
        (
            "checked brain ask \"8803\" minutes ago and it found nothing",
            3 * 3600,
        ),
        ("8803 the durable answer recorded weeks ago", 3 * 3600),
    ]);
    let answer = f.cli(&["ask", "8803"]);
    let hits = conversations(&answer);
    assert_eq!(hits.len(), 1, "{answer}");
    assert_eq!(
        hits[0]["excerpt"],
        "8803 the durable answer recorded weeks ago"
    );
    assert_eq!(answer["archive_filtered"]["recent"], 1);
    assert_eq!(answer["archive_filtered"]["self_reference"], 1);
    assert_eq!(answer["archive_hits"], 1);
    assert_budget(&answer);
    let mcp = f.mcp(json!({"query": "8803"}));
    assert_eq!(conversations(&mcp), hits);
    assert_eq!(mcp["archive_filtered"], answer["archive_filtered"]);
}

#[test]
fn archive_min_age_persists_and_zero_disables_the_age_gate() {
    let f = Fixture::new();
    f.archive_aged(&[("8803 fresh but harmless", 60)]);
    let answer = f.cli(&["ask", "8803"]);
    assert_eq!(answer["archive_hits"], 0);
    assert_eq!(answer["archive_filtered"]["recent"], 1);
    assert_eq!(answer["archive_filtered"]["self_reference"], 0);

    let saved = f.cli(&["config", "set", "archive_min_age_minutes", "0"]);
    assert_eq!(saved["archive_min_age_minutes"], 0);
    assert_eq!(f.cli(&["config", "show"])["archive_min_age_minutes"], 0);
    let all = f.cli(&["ask", "8803"]);
    assert_eq!(all["archive_hits"], 1, "{all}");
    assert_eq!(all["archive_filtered"]["recent"], 0);
    assert_budget(&all);

    f.cli(&["config", "set", "archive_min_age_minutes", "240"]);
    assert_eq!(f.cli(&["config", "show"])["archive_min_age_minutes"], 240);
    let invalid = f
        .command()
        .args(["config", "set", "archive_min_age_minutes", "soon"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(f.cli(&["config", "show"])["archive_min_age_minutes"], 240);
}

#[test]
fn malformed_archive_setting_is_fail_closed() {
    let f = Fixture::new();
    f.archive(&["8803 archived"]);
    fs::write(
        f.0.join(".brain/settings.json"),
        r#"{"version":1,"profile":"balanced","embedding_mode":"off","include_archive":"false"}"#,
    )
    .unwrap();
    for answer in [f.cli(&["ask", "8803"]), f.mcp(json!({"query":"8803"}))] {
        assert_eq!(answer["archive_included"], false);
        assert_eq!(answer["archive_hits"], 0);
        assert_eq!(results(&answer).len(), 1);
        assert!(has_archive_warning(&answer));
    }
}

#[test]
fn archive_search_and_status_understand_every_journal_role() {
    let f = Fixture::new();
    let roles = [
        "user",
        "assistant",
        "tool_call",
        "tool_result",
        "tool_error",
        "reasoning",
    ];
    let entries: Vec<(&str, &str)> = roles
        .iter()
        .map(|role| (*role, "8803 synthetic evidence"))
        .collect();
    f.archive_roles(&entries);
    for role in roles {
        let found = f.cli(&["archive", "search", "--role", role, "8803"]);
        let hits = found["results"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{role}: {found}");
        assert_eq!(hits[0]["role"], role);
    }
    let bad = f
        .command()
        .args(["archive", "search", "--role", "tool", "8803"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    let stderr = String::from_utf8_lossy(&bad.stderr);
    for role in roles {
        assert!(stderr.contains(role), "{stderr}");
    }
    let status = f.cli(&["archive", "status"]);
    let by_role = status["stored"]["by_role"].as_object().unwrap();
    for role in roles {
        assert_eq!(by_role[role], 1, "{status}");
    }
    // The MCP schema advertises the same six roles and accepts the filter.
    let reply = f.mcp_request(json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}));
    let schema = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "archive_search")
        .unwrap()["inputSchema"]["properties"]["role"]["enum"]
        .clone();
    assert_eq!(
        schema,
        json!([
            "user",
            "assistant",
            "tool_call",
            "tool_result",
            "tool_error",
            "reasoning"
        ])
    );
    let call = f.mcp_request(json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"archive_search","arguments":{"query":"8803","role":"tool_error"}}
    }));
    assert_ne!(call["result"]["isError"], true, "{call}");
    let hits = call["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["role"], "tool_error");
}

#[test]
fn ask_archive_role_flag_restricts_hits_and_keeps_real_roles() {
    let f = Fixture::new();
    f.archive_roles(&[
        ("user", "8803 the question as asked"),
        ("assistant", "8803 the answer as given"),
        ("tool_error", "8803 exit 1 permission denied"),
        ("reasoning", "8803 considering the options"),
    ]);
    let all = f.cli(&["ask", "8803"]);
    let mut seen = conversations(&all)
        .iter()
        .map(|hit| hit["role"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    seen.sort_unstable();
    assert_eq!(seen, ["assistant", "reasoning", "tool_error", "user"]);
    assert_budget(&all);

    let errors = f.cli(&["ask", "8803", "--archive-role", "tool_error"]);
    let hits = conversations(&errors);
    assert_eq!(hits.len(), 1, "{errors}");
    assert_eq!(hits[0]["role"], "tool_error");
    assert_eq!(hits[0]["excerpt"], "8803 exit 1 permission denied");
    assert_budget(&errors);

    let two = f.cli(&[
        "ask",
        "8803",
        "--archive-role",
        "user",
        "--archive-role",
        "reasoning",
    ]);
    let mut roles = conversations(&two)
        .iter()
        .map(|hit| hit["role"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    roles.sort_unstable();
    assert_eq!(roles, ["reasoning", "user"]);
    assert_budget(&two);

    let bad = f
        .command()
        .args(["ask", "8803", "--archive-role", "tool"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("tool_error"));
}

#[test]
fn period_phrase_drops_out_of_period_archive_hits_and_is_reported() {
    let f = Fixture::new();
    f.archive_dated(&[
        ("2026-10-07T10:00:00Z", "память_для_ии тогда обсуждали"),
        ("2026-09-02T10:00:00Z", "память_для_ии свежая болтовня"),
    ]);
    // Without a period both archive hits come back.
    let plain = f.cli(&["ask", "память_для_ии"]);
    assert_eq!(plain["archive_hits"], 2, "{plain}");
    assert_eq!(plain["period"], Value::Null);
    assert_eq!(plain["period_empty"], false);

    // "7 октября" names the day of the first entry only.
    let answer = f.cli(&["ask", "память_для_ии 7 октября"]);
    assert_eq!(answer["archive_hits"], 1, "{answer}");
    let conversations = conversations(&answer);
    assert_eq!(conversations[0]["timestamp"], "2026-10-07T10:00:00Z");
    assert_eq!(answer["period"]["phrase"], "7 октября");
    assert_eq!(answer["period"]["from"], 1791331200);
    assert_eq!(answer["period"]["to"], 1791417600);
    assert_eq!(answer["period_empty"], false);
    // The September hit is not merely demoted, it is gone.
    assert!(!answer.to_string().contains("свежая болтовня"));
}

#[test]
fn period_ranks_in_period_core_hits_ahead_of_the_rest() {
    let f = Fixture::new();
    f.cli(&[
        "remember",
        "память_для_ии решение принято тогда",
        "--valid-from",
        "1791374400", // 2026-10-07T12:00:00Z, inside the asked day
    ]);
    f.cli(&["remember", "память_для_ии заметка на днях"]);
    let answer = f.cli(&["ask", "память_для_ии 7 октября", "--no-archive"]);
    let hits = results(&answer);
    let position = |needle: &str| {
        hits.iter()
            .position(|hit| hit["text"].as_str().unwrap().contains(needle))
            .unwrap()
    };
    let (inside, outside) = (position("решение принято"), position("заметка на днях"));
    assert!(inside < outside, "{answer}");
    assert_eq!(answer["period"]["phrase"], "7 октября");
}

#[test]
fn earliest_question_orders_archive_hits_oldest_first() {
    let f = Fixture::new();
    f.archive_dated(&[
        ("2026-10-09T10:00:00Z", "проект_х первое обсуждение позднее"),
        ("2026-10-07T08:00:00Z", "проект_х первое обсуждение раньше"),
    ]);
    let answer = f.cli(&["ask", "первое сообщение про проект_х в октябре"]);
    let hits = conversations(&answer);
    assert_eq!(hits.len(), 2, "{answer}");
    assert_eq!(hits[0]["timestamp"], "2026-10-07T08:00:00Z");
    assert_eq!(hits[1]["timestamp"], "2026-10-09T10:00:00Z");
    assert_eq!(answer["period"]["phrase"], "октябре");
    assert_budget(&answer);
}

#[test]
fn archive_hits_carry_a_readable_utc_said_at_beside_the_raw_timestamp() {
    let f = Fixture::new();
    f.archive_dated(&[
        ("1783548804.173029", "8803 hermes время дробью"),
        ("2026-03-04T05:06:07.89Z", "8803 iso с дробью"),
        ("2026-03-04T05:06:07+02:00", "8803 iso со сдвигом"),
        ("не-время", "8803 мусорное время"),
    ]);
    let found = f.cli(&["archive", "search", "8803"]);
    let by_excerpt = |needle: &str| {
        found["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["excerpt"].as_str().unwrap().contains(needle))
            .unwrap_or_else(|| panic!("no hit for {needle}: {found}"))
            .clone()
    };
    let cases = [
        ("hermes", "1783548804.173029", "2026-07-08T22:13:24Z"),
        (
            "iso с дробью",
            "2026-03-04T05:06:07.89Z",
            "2026-03-04T05:06:07Z",
        ),
        (
            "сдвигом",
            "2026-03-04T05:06:07+02:00",
            "2026-03-04T03:06:07Z",
        ),
        ("мусорное", "не-время", "null-expect"),
    ];
    for (needle, raw, expected) in cases {
        let hit = by_excerpt(needle);
        assert_eq!(hit["timestamp"], raw, "{needle}");
        if expected == "null-expect" {
            assert_eq!(hit["said_at"], Value::Null, "{needle}: {hit}");
        } else {
            assert_eq!(hit["said_at"], expected, "{needle}: {hit}");
        }
    }

    // ask (and the MCP fusion path) keeps the same pair on every hit.
    let answer = f.cli(&["ask", "8803"]);
    for hit in conversations(&answer) {
        assert!(hit.get("said_at").is_some(), "{hit}");
        let raw = hit["timestamp"].as_str();
        let said = hit["said_at"].as_str();
        assert_eq!(said.is_some(), raw.is_some_and(epoch_ok), "{hit}");
    }
    let hermes_hit = conversations(&answer)
        .into_iter()
        .find(|hit| hit["excerpt"].as_str().unwrap().contains("время дробью"))
        .expect("hermes hit in ask");
    assert_eq!(hermes_hit["said_at"], "2026-07-08T22:13:24Z");

    // The MCP archive_search document shows it too.
    let call = f.mcp_request(json!({
        "jsonrpc":"2.0","id":7,"method":"tools/call",
        "params":{"name":"archive_search","arguments":{"query":"8803"}}
    }));
    let hits = call["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap();
    assert!(
        hits.iter().all(|hit| hit.get("said_at").is_some()),
        "{call}"
    );

    // read shows it beside the provenance copy as well.
    let read = f.cli(&["archive", "read", "1"]);
    assert_eq!(read["said_at"], "2026-07-08T22:13:24Z", "{read}");
    assert_eq!(read["timestamp"], "1783548804.173029");
}

fn epoch_ok(raw: &str) -> bool {
    crate_like_epoch_seconds(raw)
}

fn crate_like_epoch_seconds(raw: &str) -> bool {
    // Mirror of archive::epoch_seconds for the test: numeric or ISO-8601.
    if raw.parse::<f64>().is_ok() {
        return true;
    }
    raw.contains('-') && raw.contains(':')
}

#[test]
fn an_empty_period_is_reported_honestly() {
    let f = Fixture::new();
    f.archive_dated(&[("2026-09-02T10:00:00Z", "нейровеха сентябрьская правда")]);
    let answer = f.cli(&["ask", "нейровеха в сентябре 2015"]);
    assert_eq!(answer["archive_hits"], 0, "{answer}");
    assert_eq!(answer["period"]["phrase"], "сентябре 2015");
    assert_eq!(answer["period"]["from"], 1441065600);
    assert_eq!(answer["period"]["to"], 1443657600);
    assert_eq!(answer["period_empty"], true);
    let hint = answer["hint"].as_str().unwrap();
    assert!(hint.contains("period"), "{hint}");
}

#[test]
fn a_cyrillic_prefix_alone_reaches_the_archive_from_ask() {
    // Only the five-letter prefix overlaps: no Latin words anywhere.
    let f = Fixture::new();
    f.archive(&["отказ"]);
    let answer = f.cli(&["ask", "отказались"]);
    assert_eq!(answer["archive_hits"], 1, "{answer}");
    assert_eq!(conversations(&answer)[0]["excerpt"], "отказ");

    // The other direction: the stored word is the longer inflection.
    let g = Fixture::new();
    g.archive_dated(&[("2026-10-05T12:00:00Z", "октябре")]);
    let answer = g.cli(&["ask", "октября"]);
    assert_eq!(answer["archive_hits"], 1, "{answer}");
    assert_eq!(conversations(&answer)[0]["excerpt"], "октябре");
}

#[test]
fn strict_and_semantics_hold_on_cli_and_mcp_archive_search() {
    let f = Fixture::new();
    f.archive(&["alpha", "beta", "alpha beta"]);
    let cli = f.cli(&["archive", "search", "alpha beta"]);
    let hits = cli["results"].as_array().unwrap();
    assert_eq!(hits.len(), 1, "strict form must AND its terms: {cli}");
    assert_eq!(hits[0]["excerpt"], "alpha beta");

    let call = f.mcp_request(json!({
        "jsonrpc":"2.0","id":9,"method":"tools/call",
        "params":{"name":"archive_search","arguments":{"query":"alpha beta"}}
    }));
    let hits = call["result"]["structuredContent"]["results"]
        .as_array()
        .unwrap();
    assert_eq!(hits.len(), 1, "MCP strict form must AND too: {call}");
    assert_eq!(hits[0]["excerpt"], "alpha beta");
}

#[test]
fn mcp_memory_search_answers_date_phrases_like_ask() {
    let f = Fixture::new();
    f.archive_dated(&[
        ("2026-10-07T08:00:00Z", "нейровеха первое упоминание"),
        ("2026-10-09T10:00:00Z", "нейровеха поздняя сводка"),
        ("2026-09-02T10:00:00Z", "нейровеха вне периода"),
    ]);
    for query in [
        "нейровеха 7 октября",
        "нейровеха в октябре",
        "нейровеха в сентябре 2015",
        "первое сообщение про нейровеха в октябре",
    ] {
        let cli = f.cli(&["ask", query]);
        let mcp = f.mcp(json!({ "query": query }));
        assert_eq!(mcp["period"], cli["period"], "{query}");
        assert_eq!(mcp["period_empty"], cli["period_empty"], "{query}");
        assert_eq!(conversations(&mcp), conversations(&cli), "{query}");
    }

    // The period really scopes both paths, not just decorates them.
    let cli = f.cli(&["ask", "нейровеха в октябре"]);
    assert_eq!(cli["period"]["phrase"], "октябре");
    assert_eq!(conversations(&cli).len(), 2);
    assert!(!cli.to_string().contains("вне периода"));

    // An empty period says so honestly through the same hint.
    let mcp = f.mcp(json!({ "query": "нейровеха в сентябре 2015" }));
    assert_eq!(mcp["period_empty"], true);
    assert!(mcp["hint"].as_str().unwrap().contains("period"), "{mcp}");

    // "How it began" orders oldest first on both paths.
    let cli = f.cli(&["ask", "первое сообщение про нейровеха в октябре"]);
    let mcp = f.mcp(json!({ "query": "первое сообщение про нейровеха в октябре" }));
    assert_eq!(conversations(&cli)[0]["timestamp"], "2026-10-07T08:00:00Z");
    assert_eq!(conversations(&mcp)[0]["timestamp"], "2026-10-07T08:00:00Z");

    // The schema documents the date phrases.
    let reply = f.mcp_request(json!({"jsonrpc":"2.0","id":11,"method":"tools/list"}));
    let definition = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "memory_search")
        .unwrap();
    let description = definition["description"].as_str().unwrap();
    assert!(description.contains("date phrase"), "{description}");
}
