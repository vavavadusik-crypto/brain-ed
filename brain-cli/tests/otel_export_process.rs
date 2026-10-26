//! `brain archive export --format otlp-json` as a real process on a synthetic
//! archive: structure, attributes, --no-content, quarantine and date filters.
//! Only synthetic marker strings; nothing is sent anywhere.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde_json::Value;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("brain-otel-export-{}-{nonce}", std::process::id()));
        fs::create_dir_all(root.join(".brain/archive")).unwrap();
        fs::set_permissions(
            root.join(".brain/archive"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let path = root.join(".brain/archive/sessions.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:3');
             CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT, path TEXT, path_sha256 TEXT);
             INSERT INTO sources VALUES (1, 'codex', '/synthetic/state.db', 'sha');
             CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER, session_id TEXT, role TEXT,
                 locator TEXT, source_offset INTEGER, ts TEXT, content TEXT, content_sha256 TEXT,
                 evidence_sha256 TEXT, record_sha256 TEXT, imported_at TEXT, cwd TEXT);
             CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id');
             CREATE TABLE quarantine(entry_id INTEGER, reason TEXT, released_at TEXT);
             CREATE TABLE cursors(source_id INTEGER PRIMARY KEY, client TEXT NOT NULL, state TEXT NOT NULL,
                 resets INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_visit TEXT);
             CREATE TABLE collector_runs(id INTEGER PRIMARY KEY, started_at TEXT NOT NULL,
                 finished_at TEXT NOT NULL, outcome TEXT NOT NULL, summary TEXT NOT NULL);",
        )
        .unwrap();
        // Four entries: two in October, one in September, one quarantined.
        // October 2026 timestamps (Unix seconds, the Hermes storage shape).
        for (id, ts, text) in [
            (1_i64, "1790851200", "synthetic user question один"),
            (2, "1790937600", "synthetic assistant answer два"),
            (3, "1788297600", "synthetic september leftover"),
            (4, "1790862000", "synthetic tool failure"),
            (5, "1790862600", "synthetic quarantined tool_error"),
        ] {
            db.execute(
                "INSERT INTO entries VALUES (?1,1,'sess-a','user','row:1',0,?2,?3,'sha','sha','sha','2026-10-08T10:00:00Z',NULL)",
                params![id, ts, text],
            )
            .unwrap();
            db.execute(
                "INSERT INTO entries_fts(rowid, content) VALUES (?1, ?2)",
                params![id, text],
            )
            .unwrap();
        }
        db.execute("UPDATE entries SET role='user' WHERE id=2", [])
            .unwrap();
        db.execute("UPDATE entries SET role='tool_error' WHERE id=4", [])
            .unwrap();
        db.execute(
            "INSERT INTO entries_fts(rowid, content) VALUES (5, 'synthetic quarantined tool_error')",
            [],
        )
        .unwrap();
        drop(db);
        // Entry 5 goes to quarantine after the FTS mirror, like the real filter.
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute("INSERT INTO quarantine VALUES (5, 'synthetic', NULL)", [])
            .unwrap();
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_brain"));
        command
            .env("HOME", &self.0)
            .env_remove("BRAIN_DATABASE")
            .args(["--offline"]);
        command
    }

    fn export(&self, extra: &[&str]) -> Value {
        let output = self
            .command()
            .args(["archive", "export", "--format", "otlp-json"])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("valid JSON")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn all_spans(document: &Value) -> Vec<&Value> {
    document["resourceSpans"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|trace| trace["scopeSpans"].as_array().unwrap())
        .flat_map(|scope| scope["spans"].as_array().unwrap())
        .collect()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a str> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|entry| entry["key"] == key)
        .and_then(|entry| entry["value"]["stringValue"].as_str())
}

#[test]
fn export_is_valid_otlp_json_with_genai_attributes() {
    let fixture = Fixture::new();
    let document = fixture.export(&[]);
    let traces = document["resourceSpans"].as_array().unwrap();
    assert_eq!(traces.len(), 1, "one session = one trace: {document}");
    let trace = &traces[0];
    assert_eq!(
        trace["resource"]["attributes"][0]["key"], "service.name",
        "{trace}"
    );
    assert_eq!(
        trace["resource"]["attributes"][0]["value"]["stringValue"],
        "brain-ed"
    );
    assert!(trace.get("attributes").is_none());
    let spans = all_spans(&document);
    assert_eq!(spans.len(), 4, "quarantined row excluded: {document}");
    for span in &spans {
        assert_eq!(span["kind"], 1);
        assert!(span["status"]["code"].is_u64());
        assert!(attr(span, "gen_ai.request.model").is_some());
        let trace_id = span["traceId"].as_str().unwrap();
        let span_id = span["spanId"].as_str().unwrap();
        assert_eq!(trace_id.len(), 32, "OTLP traceId is 32 hex");
        assert_eq!(span_id.len(), 16, "OTLP spanId is 16 hex");
        assert!(
            trace_id
                .bytes()
                .chain(span_id.bytes())
                .all(|b| b.is_ascii_hexdigit()),
            "{span}"
        );
    }
    let user = spans
        .iter()
        .find(|span| attr(span, "brain.archive.role") == Some("user"))
        .expect("user span");
    assert_eq!(attr(user, "gen_ai.operation.name"), Some("chat"));
    assert_eq!(attr(user, "gen_ai.provider.name"), Some("codex"));
    assert_eq!(attr(user, "gen_ai.conversation.id"), Some("sess-a"));
    assert!(
        user["name"].as_str().unwrap().starts_with("chat "),
        "span name follows {{operation}} {{model}}: {user}"
    );
    assert!(
        user["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "brain.archive.content"
                && entry["value"]["stringValue"]
                    .as_str()
                    .unwrap()
                    .contains("synthetic user question")),
        "content is the masked archive text"
    );
    let messages: Value =
        serde_json::from_str(attr(user, "gen_ai.input.messages").unwrap()).unwrap();
    assert_eq!(messages[0]["parts"][0]["type"], "text");
    assert_eq!(
        messages[0]["parts"][0]["content"],
        attr(user, "brain.archive.content").unwrap()
    );
    let error = spans
        .iter()
        .find(|span| attr(span, "brain.archive.role") == Some("tool_error"))
        .expect("tool_error span");
    assert_eq!(attr(error, "gen_ai.operation.name"), Some("execute_tool"));
    assert_eq!(attr(error, "error.type"), Some("tool_error"));
    assert_eq!(error["status"]["code"], 2);
    // The document is valid JSON and mentions no network transport.
    assert!(
        document
            .to_string()
            .contains("nothing is sent over the network")
            || document["export_note"]
                .as_str()
                .unwrap()
                .contains("nothing is sent"),
        "the no-network promise is explicit"
    );
}

#[test]
fn no_content_and_filters_shape_the_export() {
    let fixture = Fixture::new();
    let no_content = fixture.export(&["--no-content"]);
    let serialized = no_content.to_string();
    assert!(
        !serialized.contains("synthetic"),
        "--no-content must not carry any text: {serialized}"
    );
    assert!(
        no_content.to_string().contains("content_omitted"),
        "{serialized}"
    );

    // Date window: October only.
    let october = fixture.export(&["--since", "2026-10-01T00:00:00Z"]);
    let october_spans = all_spans(&october);
    assert_eq!(october_spans.len(), 3, "{october}");
    let september = fixture.export(&[
        "--since",
        "2026-09-01T00:00:00Z",
        "--until",
        "2026-10-01T00:00:00Z",
    ]);
    assert_eq!(all_spans(&september).len(), 1, "{september}");
    let empty = fixture.export(&[
        "--since",
        "2020-01-01T00:00:00Z",
        "--until",
        "2020-02-01T00:00:00Z",
    ]);
    assert_eq!(empty["resourceSpans"].as_array().unwrap().len(), 0);

    // Session filter.
    let other = fixture.export(&["--session", "sess-absent"]);
    assert_eq!(other["resourceSpans"].as_array().unwrap().len(), 0);
}

#[test]
fn reasoning_is_an_assistant_message() {
    let fixture = Fixture::new();
    let db = rusqlite::Connection::open(fixture.0.join(".brain/archive/sessions.sqlite")).unwrap();
    db.execute("UPDATE entries SET role='reasoning' WHERE id=1", [])
        .unwrap();
    let document = fixture.export(&[]);
    let span = all_spans(&document)
        .into_iter()
        .find(|s| attr(s, "brain.archive.role") == Some("reasoning"))
        .unwrap();
    let messages: Value =
        serde_json::from_str(attr(span, "gen_ai.output.messages").unwrap()).unwrap();
    assert_eq!(messages[0]["role"], "assistant");
}

#[test]
fn export_uses_the_protected_archive_reader() {
    use std::os::unix::fs::symlink;
    for case in [
        "symlink",
        "permissions",
        "directory",
        "sidecar",
        "schema",
        "missing-schema",
    ] {
        let fixture = Fixture::new();
        let path = fixture.0.join(".brain/archive/sessions.sqlite");
        match case {
            "symlink" => {
                let target = fixture.0.join("synthetic.sqlite");
                fs::rename(&path, &target).unwrap();
                symlink(&target, &path).unwrap();
            }
            "permissions" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            "directory" => {
                fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o777))
                    .unwrap()
            }
            "sidecar" => symlink(&path, path.with_file_name("sessions.sqlite-wal")).unwrap(),
            "schema" | "missing-schema" => {
                let db = rusqlite::Connection::open(&path).unwrap();
                db.execute_batch(if case == "schema" {
                    "UPDATE archive_meta SET value='brain_session_archive:999'"
                } else {
                    "DROP TABLE archive_meta"
                })
                .unwrap();
            }
            _ => unreachable!(),
        }
        for args in [
            ["archive", "read", "1"],
            ["archive", "export", "--no-content"],
        ] {
            let output = fixture.command().args(args).output().unwrap();
            assert!(
                !output.status.success(),
                "{case}: {} unexpectedly succeeded",
                args[1]
            );
            assert!(output.stdout.is_empty(), "refusal must not export data");
        }
    }
}

#[test]
fn no_content_export_never_reads_the_content_column() {
    let fixture = Fixture::new();
    let path = fixture.0.join(".brain/archive/sessions.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    // A NULL content cannot be read as text: only a query that skips the column succeeds.
    db.execute_batch("UPDATE entries SET content = NULL")
        .unwrap();
    drop(db);
    let output = fixture
        .command()
        .args(["archive", "export", "--no-content"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "--no-content read the content column: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("brain.archive.content_omitted"));
}
