#[path = "common/test_support.rs"]
mod test_support;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use brain_core::embeddings::DEFAULT_MODEL2VEC_MODEL;
use serde_json::{Value, json};

fn isolated_home() -> PathBuf {
    let _nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = test_support::temp_path("brain-embedding-mode-process");
    let brain_home = root.join(".brain");
    fs::create_dir_all(&brain_home).expect("create isolated Brain home");
    fs::write(
        brain_home.join("settings.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "profile": "balanced",
            "overrides": {},
            "embedding_mode": "off",
        }))
        .expect("serialize settings"),
    )
    .expect("write settings");
    root
}

/// Stages the files the cache probe requires without using the network.
///
/// They are deliberately empty. If the runtime ignores `off`, loading fails;
/// the expected result therefore proves the mode gate runs before model I/O.
fn stage_invalid_cached_model(models_dir: &Path) {
    let repository = models_dir.join(format!(
        "models--{}",
        DEFAULT_MODEL2VEC_MODEL.replace('/', "--")
    ));
    let snapshot = repository.join("snapshots/test");
    fs::create_dir_all(&snapshot).expect("create fake model snapshot");
    fs::create_dir_all(repository.join("refs")).expect("create fake refs");
    fs::write(repository.join("refs/main"), "test").expect("write fake ref");
    for file in ["tokenizer.json", "model.safetensors", "config.json"] {
        fs::write(snapshot.join(file), b"").expect("write fake model file");
    }
}

fn brain(home: &Path, database: &Path, models_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_MODELS_DIR")
        .args([
            "--database",
            database.to_str().expect("UTF-8 database path"),
            "--models-dir",
            models_dir.to_str().expect("UTF-8 models path"),
            "--space",
            "mode-test",
            "--agent",
            "mode-test-agent",
            "--offline",
        ])
        .args(args)
        .output()
        .expect("run brain")
}

fn mcp_call(
    home: &Path,
    database: &Path,
    models_dir: &Path,
    agent: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_brain"))
        .env("HOME", home)
        .env_remove("BRAIN_DATABASE")
        .env_remove("BRAIN_MODELS_DIR")
        .args([
            "--database",
            database.to_str().expect("UTF-8 database path"),
            "--models-dir",
            models_dir.to_str().expect("UTF-8 models path"),
            "--space",
            "mode-test",
            "--agent",
            agent,
            "--offline",
            "serve",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start MCP server");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    });
    writeln!(
        child.stdin.as_mut().expect("MCP stdin"),
        "{}",
        serde_json::to_string(&request).expect("serialize MCP request")
    )
    .expect("write MCP request");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("finish MCP request");
    assert!(
        output.status.success(),
        "MCP {name} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse MCP response")
}

fn assert_no_download_advice(value: &Value) {
    let text = value.to_string().to_ascii_lowercase();
    assert!(
        text.contains("embedding mode") && text.contains("off"),
        "{value:#}"
    );
    assert!(!text.contains("model install"), "{value:#}");
    assert!(!text.contains("download"), "{value:#}");
}

fn queued_jobs(path: &Path) -> usize {
    let connection = rusqlite::Connection::open(path).expect("open vector queue");
    let count: i64 = connection
        .query_row("SELECT count(*) FROM embedding_jobs", [], |row| row.get(0))
        .expect("count vector jobs");
    usize::try_from(count).expect("non-negative vector job count")
}

#[test]
fn off_is_enforced_by_cli_mcp_and_the_worker_without_touching_the_queue() {
    let home = isolated_home();
    let brain_home = home.join(".brain");
    let database = brain_home.join("mode.sqlite");
    let models_dir = brain_home.join("models");
    stage_invalid_cached_model(&models_dir);

    let remembered = brain(
        &home,
        &database,
        &models_dir,
        &["remember", "durable memory while vectors are off"],
    );
    assert!(
        remembered.status.success(),
        "remember failed: stdout={} stderr={}",
        String::from_utf8_lossy(&remembered.stdout),
        String::from_utf8_lossy(&remembered.stderr)
    );
    let remembered_json: Value =
        serde_json::from_slice(&remembered.stdout).expect("parse remember result");
    assert_eq!(remembered_json["status"], "remembered");
    assert_no_download_advice(&remembered_json);

    let queue = brain_home.join("embedding-queue.sqlite");
    assert_eq!(queued_jobs(&queue), 1, "durable write must stay queued");
    let queue_before_worker = fs::read(&queue).expect("snapshot vector queue");
    let worker = brain(&home, &database, &models_dir, &["__worker"]);
    assert!(
        worker.status.success(),
        "worker must report disabled cleanly"
    );
    let worker_json: Value = serde_json::from_slice(&worker.stdout).expect("parse worker result");
    assert_eq!(worker_json["status"], "disabled");
    assert_eq!(worker_json["embedding_mode"], "off");
    assert_eq!(worker_json["processed"], 0);
    assert_eq!(worker_json["remaining"], 1);
    assert_eq!(
        fs::read(&queue).expect("read queue after disabled worker"),
        queue_before_worker,
        "disabled worker must not rewrite the queue"
    );
    assert!(!brain_home.join("embedding-queue.processing.jsonl").exists());
    assert!(!worker_lock_held(&brain_home));

    let searched = brain(
        &home,
        &database,
        &models_dir,
        &["ask", "durable memory while vectors are off"],
    );
    assert!(
        searched.status.success(),
        "off search must keep lexical search usable"
    );
    let searched_json: Value =
        serde_json::from_slice(&searched.stdout).expect("parse search result");
    assert_no_download_advice(&searched_json);

    let mcp_search = mcp_call(
        &home,
        &database,
        &models_dir,
        "mode-test-agent",
        "memory_search",
        json!({"query": "zzzxxyy unique absent", "space": "mode-test"}),
    );
    let mcp_search_result = &mcp_search["result"]["structuredContent"];
    assert_no_download_advice(mcp_search_result);

    let mcp_write = mcp_call(
        &home,
        &database,
        &models_dir,
        "mode-test-agent",
        "memory_write",
        json!({
            "text": "MCP write remains durable with vectors disabled",
            "space": "mode-test",
            "kind": "fact",
        }),
    );
    let mcp_write_result = &mcp_write["result"]["structuredContent"];
    assert_eq!(mcp_write_result["status"], "ok");
    assert_no_download_advice(mcp_write_result);
    assert_eq!(queued_jobs(&queue), 2);
    let memory_id = mcp_write_result["id"]
        .as_str()
        .expect("memory_write ID")
        .to_owned();

    let mcp_update = mcp_call(
        &home,
        &database,
        &models_dir,
        "mode-test-agent",
        "memory_update",
        json!({
            "id": memory_id,
            "new_text": "MCP update is durable and still waits for vectors",
            "reason": "process regression",
        }),
    );
    let mcp_update_result = &mcp_update["result"]["structuredContent"];
    assert_eq!(mcp_update_result["status"], "ok");
    assert_no_download_advice(mcp_update_result);
    assert_eq!(queued_jobs(&queue), 3);

    let connection = rusqlite::Connection::open(&database).expect("open process-test database");
    let latest_body: String = connection
        .query_row(
            "SELECT body FROM node_versions WHERE node_id = ?1 \
             ORDER BY recorded_at DESC, revision DESC LIMIT 1",
            [&memory_id],
            |row| row.get(0),
        )
        .expect("read updated durable version");
    assert_eq!(
        latest_body,
        "MCP update is durable and still waits for vectors"
    );

    let owner_write = mcp_call(
        &home,
        &database,
        &models_dir,
        "owner-a",
        "memory_write",
        json!({
            "text": "owner A keeps this memory",
            "space": "mode-test",
            "kind": "decision",
        }),
    );
    let owner_id = owner_write["result"]["structuredContent"]["id"]
        .as_str()
        .expect("owner memory ID")
        .to_owned();
    let versions_before: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_versions WHERE node_id = ?1",
            [&owner_id],
            |row| row.get(0),
        )
        .expect("count owner versions before attack");

    for (tool, arguments) in [
        (
            "memory_update",
            json!({"id": owner_id.clone(), "new_text": "intruder replacement"}),
        ),
        (
            "memory_forget",
            json!({"ids": [owner_id.clone()], "reason": "intruder deletion"}),
        ),
    ] {
        let denied = mcp_call(&home, &database, &models_dir, "intruder-b", tool, arguments);
        let denied_result = &denied["result"]["structuredContent"];
        assert_eq!(denied_result["status"], "error", "{denied_result:#}");
        assert_eq!(denied["result"]["isError"], true);
    }
    let versions_after: i64 = connection
        .query_row(
            "SELECT count(*) FROM node_versions WHERE node_id = ?1",
            [&owner_id],
            |row| row.get(0),
        )
        .expect("count owner versions after denied mutations");
    let owner_body: String = connection
        .query_row(
            "SELECT body FROM node_versions WHERE node_id = ?1 \
             ORDER BY recorded_at DESC, revision DESC LIMIT 1",
            [&owner_id],
            |row| row.get(0),
        )
        .expect("read owner memory after denied mutations");
    assert_eq!(versions_after, versions_before);
    assert_eq!(owner_body, "owner A keeps this memory");
    assert!(!brain_home.join("embedding-queue.processing.jsonl").exists());
    assert!(!worker_lock_held(&brain_home));

    fs::remove_dir_all(home).expect("remove isolated Brain home");
}

/// The worker lock file persists; a worker runs exactly while it holds the lock.
fn worker_lock_held(brain_home: &Path) -> bool {
    fs::OpenOptions::new()
        .read(true)
        .open(brain_home.join("embedding-worker.lock"))
        .is_ok_and(|file| match file.try_lock() {
            Ok(()) => {
                // Unlock explicitly: a child forked meanwhile shares this
                // descriptor until it execs and would keep the lock.
                let _ = file.unlock();
                false
            }
            Err(error) => matches!(error, fs::TryLockError::WouldBlock),
        })
}
