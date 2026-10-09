//! What is in the way, and where the work stood.
//!
//! Storing and searching is not the whole job an operational memory does. An
//! agent picking up work needs two answers no search query gives well: what is
//! currently blocking, and what state the last session left behind. Both are
//! already in the records — a task nobody closed, an error nothing superseded —
//! and neither was reachable without knowing what to search for.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::Timestamp;

/// Age past which an open item is stale rather than blocking.
///
/// A task left open for two months is not what is stopping today's work; it is
/// something that was abandoned. Reported separately so a genuine blocker list
/// does not drown in history.
const STALE_AFTER: Timestamp = 60 * 24 * 60 * 60;

/// Lists what is currently in the way.
///
/// A blocker is an unresolved task, error, or gotcha: recorded, never
/// superseded, and not marked done. Ordered by recency, because the thing that
/// blocked yesterday is more likely to block now than the thing from March.
///
/// # Errors
///
/// Returns an error when the query fails.
pub fn blockers(
    connection: &Connection,
    space_id: &str,
    now: Timestamp,
    limit: usize,
) -> rusqlite::Result<Value> {
    let limit = limit.clamp(1, 200);
    let mut statement = connection.prepare(
        r#"SELECT v.node_id, v.label, v.body, v.kind, v.recorded_at, v.metadata
           FROM node_versions AS v
           WHERE v.space_id = ?1
             AND v.superseded_at IS NULL
             AND (v.valid_to IS NULL OR v.valid_to > ?2)
             AND v.kind IN ('error', 'decision', 'memory')
           ORDER BY v.recorded_at DESC
           LIMIT 500"#,
    )?;

    let rows = statement
        .query_map(rusqlite::params![space_id, now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut open = Vec::new();
    let mut stale = 0usize;
    for (id, label, body, kind, recorded_at, metadata) in rows {
        let memory_kind = serde_json::from_str::<Value>(&metadata)
            .ok()
            .and_then(|value| {
                value
                    .get("memory")
                    .and_then(|memory| memory.get("kind"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| kind.clone());
        if !matches!(memory_kind.as_str(), "task" | "error" | "gotcha") {
            continue;
        }
        if looks_resolved(&body) {
            continue;
        }
        let age = now.saturating_sub(recorded_at).max(0);
        if age > STALE_AFTER {
            stale += 1;
            continue;
        }
        open.push(json!({
            "id": id,
            "kind": memory_kind,
            "title": first_line(&label, &body),
            "excerpt": excerpt(&body),
            "recorded_at": recorded_at,
            "age_days": age / 86_400,
        }));
        if open.len() >= limit {
            break;
        }
    }

    Ok(json!({
        "status": if open.is_empty() { "clear" } else { "blocked" },
        "blockers": open,
        "count": open.len(),
        // Named rather than hidden: an item nobody has touched in two months is
        // not what is blocking today, but pretending it does not exist would be
        // its own kind of lie.
        "stale_ignored": stale,
        "stale_after_days": STALE_AFTER / 86_400,
    }))
}

/// A record whose text says the work is finished.
///
/// Deliberately conservative: only phrases that state completion outright
/// count, because wrongly hiding a live blocker is worse than listing a closed
/// one.
fn looks_resolved(body: &str) -> bool {
    const MARKERS: [&str; 10] = [
        "решено",
        "исправлено",
        "закрыто",
        "готово",
        "починено",
        "resolved",
        "fixed",
        "closed",
        "done:",
        "no longer",
    ];
    let head = body.chars().take(200).collect::<String>().to_lowercase();
    MARKERS.iter().any(|marker| head.contains(marker))
}

/// Summarises where the work stands, for an agent that just arrived.
///
/// # Errors
///
/// Returns an error when the query fails.
pub fn checkpoint(
    connection: &Connection,
    space_id: &str,
    now: Timestamp,
) -> rusqlite::Result<Value> {
    let mut statement = connection.prepare(
        r#"SELECT v.kind, count(*), max(v.recorded_at)
           FROM node_versions AS v
           WHERE v.space_id = ?1 AND v.superseded_at IS NULL
           GROUP BY v.kind"#,
    )?;
    let mut counts = HashMap::new();
    let mut latest = 0i64;
    for row in statement.query_map([space_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })? {
        let (kind, count, newest) = row?;
        latest = latest.max(newest);
        counts.insert(kind, count);
    }
    drop(statement);

    // What changed in the last day: the answer to "what was I doing", which a
    // total count cannot give.
    let since = now.saturating_sub(24 * 60 * 60);
    let mut statement = connection.prepare(
        r#"SELECT v.label, v.kind, v.recorded_at
           FROM node_versions AS v
           WHERE v.space_id = ?1 AND v.superseded_at IS NULL AND v.recorded_at >= ?2
           ORDER BY v.recorded_at DESC
           LIMIT 12"#,
    )?;
    let recent = statement
        .query_map(rusqlite::params![space_id, since], |row| {
            Ok(json!({
                "title": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "recorded_at": row.get::<_, i64>(2)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let blocking = blockers(connection, space_id, now, 5)?;

    Ok(json!({
        "status": "ok",
        "space": space_id,
        "records_by_kind": counts,
        "latest_recorded_at": latest,
        "idle_seconds": now.saturating_sub(latest).max(0),
        "recent": recent,
        "recent_window_hours": 24,
        "blockers": blocking["blockers"],
        "blocker_count": blocking["count"],
    }))
}

/// The first meaningful line, preferring a label that says something.
fn first_line(label: &str, body: &str) -> String {
    let trimmed = label.trim();
    // Section headers reused across an archive make useless titles; the body's
    // opening line is what the record is actually about. Compared in lowercase
    // rather than with the ASCII variant, which leaves Cyrillic case untouched
    // and let "Память" through as a title.
    let lowered = trimmed.to_lowercase();
    const USELESS: [&str; 6] = ["память", "заметка", "note", "memory", "запись", "untitled"];
    if trimmed.chars().count() > 3 && !USELESS.contains(&lowered.as_str()) {
        return trimmed.to_owned();
    }
    body.lines()
        .map(str::trim)
        .find(|line| line.len() > 3)
        .unwrap_or(trimmed)
        .to_owned()
}

fn excerpt(body: &str) -> String {
    const MAX_CHARS: usize = 160;
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let kept = flat.chars().take(MAX_CHARS - 1).collect::<String>();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_outright_completion_counts_as_resolved() {
        assert!(looks_resolved("Решено: перешли на другой порт"));
        assert!(looks_resolved("Fixed by rebuilding the index"));
        // Hiding a live blocker is worse than listing a closed one, so anything
        // short of a plain statement of completion stays on the list.
        assert!(!looks_resolved("Надо будет починить, пока не понятно как"));
        assert!(!looks_resolved("Похоже, скоро закроем этот вопрос"));
    }

    #[test]
    fn a_useless_label_falls_back_to_the_body() {
        assert_eq!(
            first_line("Память", "Перегрев ноутбука при сборке"),
            "Перегрев ноутбука при сборке"
        );
        assert_eq!(
            first_line("Понятный заголовок", "тело"),
            "Понятный заголовок"
        );
    }

    #[test]
    fn a_long_excerpt_is_cut_and_says_so() {
        let long = "слово ".repeat(100);
        let short = excerpt(&long);

        assert!(short.chars().count() <= 160);
        assert!(short.ends_with('…'));
    }
}
