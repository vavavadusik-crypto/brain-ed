//! `brain maintenance redact`: mask secrets already stored in brain.sqlite.
//!
//! The Markdown importer once stored whole lines bare and lines containing
//! credentials whole; the archive masker has always been stricter. This pass
//! brings every stored `node_versions` row — history included, an explicit
//! decision for secrets — up to the same bar as `redact::mask_secrets`.
//!
//! Dry-run prints only counters (never text: a real base holds credentials).
//! `--apply` first writes a consistent `0600` snapshot beside the database
//! (`VACUUM INTO`: a plain file copy of a WAL database can miss committed pages),
//! then rewrites the touched rows in one transaction; the FTS index follows through
//! its own triggers, and the touched versions' embedding records are deleted
//! so the ordinary `brain reindex` recomputes them. The old text must not survive
//! in the file either: deletes run with `secure_delete`, the FTS index is
//! optimized, the WAL is checkpointed and the file is vacuumed. A second
//! `--apply` finds nothing.

use std::error::Error;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

/// One row worth rewriting: its rowid and the masked replacements.
struct Touched {
    row_id: i64,
    label: String,
    body: String,
}

fn open_writable(database: &Path) -> Result<Connection, Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(connection)
}

/// Rows whose label or body changes under the masker, with the new text.
fn touched_rows(connection: &Connection) -> Result<Vec<(Touched, String)>, Box<dyn Error>> {
    let mut statement = connection
        .prepare("SELECT row_id, label, body, source FROM node_versions ORDER BY row_id")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                // Memories written by agents have no source file.
                row.get::<_, Option<String>>(3)?
                    .unwrap_or_else(|| "(no source)".to_owned()),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut touched = Vec::new();
    for (row_id, label, body, source) in rows {
        let (masked_label, label_changed) = crate::redact::mask_secrets(&label);
        let (masked_body, body_changed) = crate::redact::mask_secrets(&body);
        if label_changed || body_changed {
            touched.push((
                Touched {
                    row_id,
                    label: masked_label,
                    body: masked_body,
                },
                source,
            ));
        }
    }
    Ok(touched)
}

/// Deletes the embedding records of the touched versions (vectors first), so
/// the ordinary reindex pass recomputes them from the new text.
fn drop_embeddings(connection: &Connection, row_ids: &[i64]) -> Result<usize, Box<dyn Error>> {
    let mut dropped = 0;
    for row_id in row_ids {
        let mut statement = connection
            .prepare("SELECT id, dimensions FROM embedding_records WHERE node_version_id = ?1")?;
        let records: Vec<(i64, i64)> = statement
            .query_map(params![row_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (record_id, dimensions) in records {
            let table = match dimensions {
                256 => "node_embeddings_vec_256",
                384 => "node_embeddings_vec_384",
                _ => continue, // unknown dimension set: reindex will reconcile
            };
            connection.execute(
                &format!("DELETE FROM {table} WHERE embedding_id = ?1"),
                params![record_id],
            )?;
            connection.execute(
                "DELETE FROM embedding_records WHERE id = ?1",
                params![record_id],
            )?;
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// Sibling `<db>.pre-redact-<unix>` snapshot, owner-only from its first byte.
fn backup(connection: &Connection, database: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let target = database.with_extension(format!("pre-redact-{stamp}"));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&target)?;
    }
    let path = target.to_str().ok_or("backup path is not UTF-8")?;
    connection.execute("VACUUM INTO ?1", params![path])?;
    Ok(target)
}

/// Leaves no copy of the replaced text in the file: optimized FTS segments, a
/// truncated WAL and a vacuumed main file. Busy readers only delay the vacuum,
/// so its outcome is reported rather than assumed.
fn purge_old_pages(connection: &Connection) -> Value {
    let step = |sql: &str| match connection.execute_batch(sql) {
        Ok(()) => json!("ok"),
        Err(error) => json!(format!("failed: {error}")),
    };
    json!({
        "fts_optimize": step("INSERT INTO node_versions_fts(node_versions_fts) VALUES ('optimize')"),
        "checkpoint": step("PRAGMA wal_checkpoint(TRUNCATE)"),
        "vacuum": step("VACUUM"),
        "checkpoint_after_vacuum": step("PRAGMA wal_checkpoint(TRUNCATE)"),
    })
}

pub(crate) fn redact_document(database: &Path, apply: bool) -> Result<Value, Box<dyn Error>> {
    // A dry run needs no write access; apply does.
    let connection = if apply {
        // Opening the storage once registers sqlite-vec for every later connection
        // in this process: the vector tables are `vec0` and cannot be written without it.
        drop(crate::open_storage(database)?);
        open_writable(database)?
    } else {
        Connection::open_with_flags(
            database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?
    };
    let touched = touched_rows(&connection)?;
    let mut by_source: std::collections::BTreeMap<String, i64> = Default::default();
    for (_, source) in &touched {
        *by_source.entry(source.clone()).or_default() += 1;
    }
    let versions = touched.len();
    if !apply || versions == 0 {
        return Ok(json!({
            "status": "ok",
            "apply": apply,
            "versions": versions,
            "by_source": by_source,
            "changed": false,
            "note": if apply {
                "nothing to mask: every stored version already matches the masker"
            } else {
                "dry run: nothing written; run brain maintenance redact --apply to rewrite"
            },
        }));
    }

    let backup_path = backup(&connection, database)?;
    // Freed cells are zeroed, so the replaced text is not left inside live pages.
    connection.pragma_update(None, "secure_delete", true)?;
    connection.execute("BEGIN IMMEDIATE", [])?;
    let write = (|| -> Result<(usize, usize), Box<dyn Error>> {
        // Judged again under the write lock: an import may have run since the count.
        let touched = touched_rows(&connection)?;
        let row_ids: Vec<i64> = touched.iter().map(|(row, _)| row.row_id).collect();
        let mut written = 0;
        {
            let mut statement = connection
                .prepare("UPDATE node_versions SET label = ?1, body = ?2 WHERE row_id = ?3")?;
            for (row, _) in &touched {
                written += statement.execute(params![row.label, row.body, row.row_id])?;
            }
        }
        let dropped = drop_embeddings(&connection, &row_ids)?;
        connection.execute("COMMIT", [])?;
        Ok((written, dropped))
    })();
    let (written, dropped) = match write {
        Ok(counts) => counts,
        Err(error) => {
            if !connection.is_autocommit() {
                let _ = connection.execute("ROLLBACK", []);
            }
            return Err(error);
        }
    };
    let purge = purge_old_pages(&connection);
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    Ok(json!({
        "status": "ok",
        "apply": true,
        "versions": versions,
        "by_source": by_source,
        "changed": true,
        "backup": backup_path,
        "versions_written": written,
        "embeddings_dropped": dropped,
        "purge": purge,
        "integrity": integrity,
        "note": "history rewritten on purpose: secrets must not survive in superseded versions",
    }))
}
