//! `brain maintenance redact` as a real process: a synthetic base with a
//! secret in two versions of one node — dry run counts, apply rewrites node
//! versions and the FTS index, search shows the key but not the value, and a
//! second apply finds nothing. Only synthetic marker strings, no real data.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use serde_json::{Value, json};

fn home(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "brain-maintenance-{tag}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(root.join(".brain")).unwrap();
    root
}

fn brain(home: &std::path::Path, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .args(["--offline", "--space", "default", "--agent", "t"])
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

fn database(home: &std::path::Path) -> PathBuf {
    home.join(".brain/brain.sqlite")
}

/// Seeds one node with two versions; the second (current) one supersedes the
/// first, so the secret sits in live text and in history.
fn seed(home: &std::path::Path, secret_value: &str) {
    let connection = rusqlite::Connection::open(database(home)).unwrap();
    connection
        .execute_batch(
            "INSERT INTO agents(id, display_name, created_at) VALUES ('t', 't', 1);
             INSERT INTO spaces(id, kind, label, created_by_agent_id, created_at)
               VALUES ('default', 'project', 'default', 't', 1);
             INSERT INTO sessions(id, space_id, agent_id, started_at) VALUES ('s', 'default', 't', 1);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id,
                               created_at, last_accessed, degree)
             VALUES ('n1', 'default', 't', 's', 1, 1, 1)",
            [],
        )
        .unwrap();
    for (row_id, superseded) in [(1_i64, Some(5_i64)), (2, None)] {
        let body = format!("the deploy key OPENAI_API_KEY={secret_value} is set in ci\n");
        connection
            .execute(
                "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, source,
                                           valid_from, valid_to, recorded_at, superseded_at,
                                           agent_id, session_id)
                 VALUES ('n1', 'default', ?1, 'memory', 'deploy key note', ?2,
                         'docs/deploy.md', 1, NULL, 1, ?3, 't', 's')",
                params![row_id, body, superseded],
            )
            .unwrap();
    }
    // Mirror the rows into FTS the way the storage layer's triggers would.
    for row_id in [1, 2] {
        connection
            .execute(
                "INSERT INTO node_versions_fts(rowid, label, body)
                 SELECT row_id, label, body FROM node_versions WHERE row_id = ?1",
                params![row_id],
            )
            .unwrap();
    }
    drop(connection);
}

#[test]
fn maintenance_redact_counts_rewrites_and_is_idempotent() {
    // Assembled so scanners never see a full literal.
    let secret_value = format!("sk-{}{}", "ant-", "A1b2C3d4E5f6G7h8I9j0K1");
    let home = home("redact");
    let (ok, _) = brain(&home, &["init"]);
    assert!(ok, "init must succeed");
    seed(&home, &secret_value);

    // Dry run: counters only, nothing written.
    let (ok, dry) = brain(&home, &["maintenance", "redact"]);
    assert!(ok, "{}", dry);
    assert_eq!(dry["apply"], false);
    assert_eq!(dry["versions"], 2, "{dry}");
    assert_eq!(dry["by_source"]["docs/deploy.md"], 2);
    assert_eq!(dry["changed"], false);

    // The file is untouched by the dry run.
    let connection = rusqlite::Connection::open(database(&home)).unwrap();
    let bare: String = connection
        .query_row(
            "SELECT body FROM node_versions WHERE row_id = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(bare.contains(&secret_value), "dry run must not rewrite");

    // Apply: both versions rewritten in place.
    let (ok, applied) = brain(&home, &["maintenance", "redact", "--apply"]);
    assert!(ok, "{}", applied);
    assert_eq!(applied["apply"], true);
    assert_eq!(applied["versions"], 2);
    assert_eq!(applied["integrity"], "ok");
    let backup = applied["backup"].as_str().unwrap();
    assert!(fs::metadata(backup).is_ok(), "backup file exists");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // node_versions: the key survives, the value is gone, in BOTH versions.
    for row_id in [1, 2] {
        let body: String = connection
            .query_row(
                "SELECT body FROM node_versions WHERE row_id = ?1",
                params![row_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(body.contains("OPENAI_API_KEY"), "{body}");
        assert!(!body.contains(&secret_value), "row {row_id} leaked: {body}");
        assert!(body.contains("[REDACTED]"), "{body}");
    }
    // FTS no longer matches the value and still matches the key.
    let value_hits: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_versions_fts WHERE node_versions_fts MATCH ?1",
            [format!("\"{}\"", secret_value).as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value_hits, 0, "the value must be out of the index");
    let key_hits: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_versions_fts WHERE node_versions_fts MATCH ?1",
            ["openai_api_key"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(key_hits, 2, "the key stays searchable");
    drop(connection);

    // Search (ask path) finds the note by its key and shows no value.
    let (ok, found) = brain(&home, &["ask", "OPENAI_API_KEY"]);
    assert!(ok, "{}", found);
    let text = found.to_string();
    assert!(text.contains("OPENAI_API_KEY"), "{found}");
    assert!(text.contains("[REDACTED]"), "{found}");
    assert!(!text.contains(&secret_value), "search leaked the value");

    // Idempotent: the second apply reports nothing to do.
    let (ok, again) = brain(&home, &["maintenance", "redact", "--apply"]);
    assert!(ok, "{}", again);
    assert_eq!(again["versions"], 0, "{again}");
    assert_eq!(again["changed"], false);

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn apply_leaves_no_byte_of_the_secret_in_the_file_and_snapshots_it_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let secret_value = format!("sk-{}{}", "ant-", "Z9y8X7w6V5u4T3s2R1q0P9");
    let home = home("purge");
    let (ok, _) = brain(&home, &["init"]);
    assert!(ok, "init must succeed");
    // A live agent (`brain serve`) keeps the database open, so committed pages stay
    // in the WAL instead of the main file: a plain file copy would miss them.
    let holder = rusqlite::Connection::open(database(&home)).unwrap();
    holder.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    seed(&home, &secret_value);
    let (ok, applied) = brain(&home, &["maintenance", "redact", "--apply"]);
    assert!(ok, "{applied}");
    assert_eq!(applied["versions_written"], 2, "{applied}");
    for step in [
        "fts_optimize",
        "checkpoint",
        "vacuum",
        "checkpoint_after_vacuum",
    ] {
        assert_eq!(applied["purge"][step], "ok", "{applied}");
    }
    // Neither the main file nor a WAL beside it may still hold the replaced text.
    let db = database(&home);
    for path in [db.clone(), db.with_extension("sqlite-wal")] {
        if let Ok(bytes) = fs::read(&path) {
            assert!(
                !bytes
                    .windows(secret_value.len())
                    .any(|w| w == secret_value.as_bytes()),
                "{} still holds the secret",
                path.display()
            );
        }
    }
    // The snapshot is a whole database, readable by the owner only.
    let backup = PathBuf::from(applied["backup"].as_str().unwrap());
    assert_eq!(
        fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let snapshot = rusqlite::Connection::open(&backup).unwrap();
    let kept: i64 = snapshot
        .query_row("SELECT count(*) FROM node_versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 2, "the snapshot must hold the rows still in the WAL");
    drop(snapshot);
    drop(holder);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn a_memory_without_a_source_file_is_counted_and_masked() {
    let secret_value = format!("sk-{}{}", "ant-", "M1n2B3v4C5x6Z7l8K9j0H1");
    let home = home("nosource");
    let (ok, _) = brain(&home, &["init"]);
    assert!(ok, "init must succeed");
    seed(&home, &secret_value);
    let connection = rusqlite::Connection::open(database(&home)).unwrap();
    connection
        .execute("UPDATE node_versions SET source = NULL", [])
        .unwrap();
    // A stored vector for a touched version: removing it goes through the vec0 tables.
    connection
        .execute(
            "INSERT INTO embedding_records(node_version_id, space_id, model_id, dimensions,
                                           embedded_at, agent_id, session_id)
             VALUES (2, 'default', 'test-model', 256, 1, 't', 's')",
            [],
        )
        .unwrap();
    drop(connection);
    let (ok, dry) = brain(&home, &["maintenance", "redact"]);
    assert!(ok, "{dry}");
    assert_eq!(dry["by_source"]["(no source)"], 2, "{dry}");
    let (ok, applied) = brain(&home, &["maintenance", "redact", "--apply"]);
    assert!(ok, "{applied}");
    assert_eq!(applied["versions_written"], 2, "{applied}");
    assert_eq!(applied["embeddings_dropped"], 1, "{applied}");
    let _ = fs::remove_dir_all(&home);
}
