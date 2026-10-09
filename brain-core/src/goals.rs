//! What the person is working towards.
//!
//! The last thing on the market survey that no competitor does: a memory that
//! knows not only what was said but what it was all for. Mem0, Zep, Letta,
//! Cognee and the rest store and retrieve; none of them can answer "what am I
//! trying to do here".
//!
//! Three sources, by the owner's decision, and the answer says which one it
//! came from. A goal the tool inferred and a goal the person stated are not
//! the same kind of claim, and presenting them identically would make the
//! honest one indistinguishable from the guess.

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::Timestamp;

/// Records examined when inferring. Beyond this the oldest are noise.
const SCAN_LIMIT: usize = 400;

/// Mentions below which a recurring subject is a coincidence, not a goal.
///
/// Two records sharing a word is how language works; four is a pattern. Set
/// low enough to find real work and high enough that one busy afternoon does
/// not become a life's purpose.
const MIN_MENTIONS: usize = 4;

/// Days after which an untouched goal is dormant rather than active.
const DORMANT_AFTER_DAYS: i64 = 21;

/// Where a goal came from.
///
/// Kept in the output rather than flattened away: an inference and a statement
/// carry different weight, and the reader is entitled to know which they have.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalSource {
    /// Derived from what the records keep returning to.
    Inferred,
    /// Written down by the person, and therefore not up for debate.
    Stated,
}

impl GoalSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Inferred => "выведена из записей",
            Self::Stated => "задана человеком",
        }
    }
}

/// Records a goal the person stated outright.
///
/// A stated goal supersedes any inference about the same subject: the tool
/// guessing what someone is doing is worth less than them saying it.
///
/// # Errors
///
/// Returns an error when the write fails.
pub fn set(
    connection: &Connection,
    space_id: &str,
    text: &str,
    now: Timestamp,
) -> rusqlite::Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(json!({
            "status": "error",
            "message": "цель не может быть пустой",
        }));
    }
    connection.execute(
        r#"CREATE TABLE IF NOT EXISTS stated_goals (
               space_id   TEXT NOT NULL,
               text       TEXT NOT NULL,
               stated_at  INTEGER NOT NULL,
               cleared_at INTEGER
           )"#,
        [],
    )?;
    // The previous goal is closed rather than overwritten: what someone was
    // working towards last month is part of the record, not a mistake.
    connection.execute(
        "UPDATE stated_goals SET cleared_at = ?2 WHERE space_id = ?1 AND cleared_at IS NULL",
        params![space_id, now],
    )?;
    connection.execute(
        "INSERT INTO stated_goals(space_id, text, stated_at) VALUES (?1, ?2, ?3)",
        params![space_id, trimmed, now],
    )?;
    Ok(json!({
        "status": "ok",
        "goal": trimmed,
        "source": GoalSource::Stated.as_str(),
        "stated_at": now,
    }))
}

/// Withdraws the stated goal, leaving inference to speak again.
///
/// The record is closed rather than deleted, exactly as `set` does when
/// replacing one: what someone was working towards is part of the history even
/// once they stop. Without this, a goal stated by mistake could only be
/// replaced, never taken back.
///
/// # Errors
///
/// Returns an error when the write fails.
pub fn clear(connection: &Connection, space_id: &str, now: Timestamp) -> rusqlite::Result<Value> {
    let exists: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'stated_goals'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(json!({"status": "ok", "cleared": 0, "note": "цель и не была задана"}));
    }
    let cleared = connection.execute(
        "UPDATE stated_goals SET cleared_at = ?2 WHERE space_id = ?1 AND cleared_at IS NULL",
        params![space_id, now],
    )?;
    Ok(json!({
        "status": "ok",
        "cleared": cleared,
        "note": if cleared == 0 {
            "цель и не была задана"
        } else {
            "цель снята; теперь она снова выводится из записей"
        },
    }))
}

/// Reports what the person appears to be working towards.
///
/// # Errors
///
/// Returns an error when the query fails.
pub fn goals(connection: &Connection, space_id: &str, now: Timestamp) -> rusqlite::Result<Value> {
    let stated = stated_goal(connection, space_id)?;
    let inferred = infer(connection, space_id, now)?;

    let confidence = if stated.is_some() {
        "точно"
    } else if inferred.len() >= 2 {
        "предположительно"
    } else {
        "недостаточно данных"
    };

    Ok(json!({
        "status": "ok",
        "stated": stated,
        "inferred": inferred,
        "confidence": confidence,
        // Said plainly, because a tool that confidently announces the wrong
        // purpose is worse than one that admits it does not know. When nothing
        // was stated and little was inferred, the honest answer is to ask.
        "hint": if stated.is_none() && inferred.len() < 2 {
            "записей мало для вывода цели — задайте её командой `brain goal set`"
        } else if stated.is_none() {
            "цель выведена из записей и может быть неточной — уточните командой `brain goal set`"
        } else {
            "цель задана явно"
        },
    }))
}

fn stated_goal(connection: &Connection, space_id: &str) -> rusqlite::Result<Option<Value>> {
    let exists: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'stated_goals'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        r#"SELECT text, stated_at FROM stated_goals
           WHERE space_id = ?1 AND cleared_at IS NULL
           ORDER BY stated_at DESC LIMIT 1"#,
    )?;
    let mut rows = statement.query([space_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(json!({
        "text": row.get::<_, String>(0)?,
        "stated_at": row.get::<_, i64>(1)?,
        "source": GoalSource::Stated.as_str(),
    })))
}

/// Subjects the recent records keep returning to.
///
/// Frequency alone would surface whatever word is common in this language;
/// what marks a goal is a subject that recurs across *separate days* and
/// carries decisions or tasks rather than passing mentions.
fn infer(connection: &Connection, space_id: &str, now: Timestamp) -> rusqlite::Result<Vec<Value>> {
    let mut statement = connection.prepare(
        r#"SELECT v.label, v.body, v.kind, v.recorded_at
           FROM node_versions AS v
           WHERE v.space_id = ?1 AND v.superseded_at IS NULL
             AND v.kind IN ('decision', 'memory', 'error')
           ORDER BY v.recorded_at DESC
           LIMIT ?2"#,
    )?;
    let rows = statement
        .query_map(params![space_id, SCAN_LIMIT as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut subjects: std::collections::HashMap<String, Subject> = std::collections::HashMap::new();
    for (label, body, _kind, recorded_at) in rows {
        let day = recorded_at / 86_400;
        for term in terms(&format!("{label} {body}")) {
            let subject = subjects.entry(term).or_default();
            subject.mentions += 1;
            subject.days.insert(day);
            subject.newest = subject.newest.max(recorded_at);
        }
    }

    let mut found = subjects
        .into_iter()
        .filter(|(_, subject)| {
            // Recurring across days, not merely repeated within one sitting.
            subject.mentions >= MIN_MENTIONS && subject.days.len() >= 2
        })
        .map(|(term, subject)| {
            let idle_days = now.saturating_sub(subject.newest).max(0) / 86_400;
            json!({
                "subject": term,
                "mentions": subject.mentions,
                "days_active": subject.days.len(),
                "last_touched_days_ago": idle_days,
                "state": if idle_days > DORMANT_AFTER_DAYS { "затихло" } else { "в работе" },
                "source": GoalSource::Inferred.as_str(),
            })
        })
        .collect::<Vec<_>>();

    found.sort_by(|left, right| {
        let key = |value: &Value| {
            (
                value["days_active"].as_u64().unwrap_or(0),
                value["mentions"].as_u64().unwrap_or(0),
            )
        };
        key(right).cmp(&key(left))
    });
    found.truncate(6);
    Ok(found)
}

#[derive(Default)]
struct Subject {
    mentions: usize,
    days: std::collections::HashSet<i64>,
    newest: Timestamp,
}

/// Content words long enough to name a subject.
///
/// Short and structural words are dropped: a goal named "этот" or "which" is
/// no goal at all, and letting them through would make the commonest words in
/// the language look like someone's life's work.
fn terms(text: &str) -> Vec<String> {
    const STOP: [&str; 24] = [
        "который",
        "которая",
        "которые",
        "потому",
        "чтобы",
        "нужно",
        "может",
        "будет",
        "этого",
        "этому",
        "также",
        "после",
        "перед",
        "через",
        "около",
        "здесь",
        "which",
        "because",
        "should",
        "would",
        "there",
        "where",
        "these",
        "those",
    ];
    text.split(|character: char| !character.is_alphanumeric() && character != '-')
        .map(|word| word.trim_matches('-').to_lowercase())
        .filter(|word| word.chars().count() >= 5 && word.chars().count() <= 24)
        .filter(|word| !STOP.contains(&word.as_str()))
        .filter(|word| word.chars().any(char::is_alphabetic))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_words_never_become_goals() {
        let found = terms("который нужно сделать чтобы перегрев ноутбука прекратился");

        assert!(!found.contains(&"который".to_owned()));
        assert!(!found.contains(&"нужно".to_owned()));
        assert!(found.contains(&"перегрев".to_owned()));
    }

    #[test]
    fn short_words_are_not_subjects() {
        let found = terms("код баг тест перегрев");

        assert!(!found.contains(&"код".to_owned()));
        assert!(!found.contains(&"баг".to_owned()));
        assert!(found.contains(&"перегрев".to_owned()));
    }

    /// A tool that confidently announces the wrong purpose is worse than one
    /// that says it does not know.
    #[test]
    fn an_empty_memory_admits_it_cannot_tell() {
        let connection = Connection::open_in_memory().expect("memory database");
        connection
            .execute(
                r#"CREATE TABLE node_versions (
                       space_id TEXT, label TEXT, body TEXT, kind TEXT,
                       recorded_at INTEGER, superseded_at INTEGER
                   )"#,
                [],
            )
            .expect("schema");

        let report = goals(&connection, "default", 1_000_000).expect("goals");

        assert_eq!(report["confidence"], "недостаточно данных");
        assert!(report["stated"].is_null());
        assert!(
            report["hint"]
                .as_str()
                .is_some_and(|text| text.contains("goal set")),
            "an uncertain answer must say how to resolve the uncertainty"
        );
    }

    #[test]
    fn a_stated_goal_outranks_whatever_was_inferred() {
        let connection = Connection::open_in_memory().expect("memory database");
        connection
            .execute(
                r#"CREATE TABLE node_versions (
                       space_id TEXT, label TEXT, body TEXT, kind TEXT,
                       recorded_at INTEGER, superseded_at INTEGER
                   )"#,
                [],
            )
            .expect("schema");

        set(&connection, "default", "  выпустить продукт  ", 500).expect("set");
        let report = goals(&connection, "default", 1_000).expect("goals");

        assert_eq!(report["stated"]["text"], "выпустить продукт");
        assert_eq!(report["confidence"], "точно");
    }

    #[test]
    fn setting_a_new_goal_closes_the_old_one_instead_of_erasing_it() {
        let connection = Connection::open_in_memory().expect("memory database");
        set(&connection, "default", "первая цель", 100).expect("first");
        set(&connection, "default", "вторая цель", 200).expect("second");

        let open: i64 = connection
            .query_row(
                "SELECT count(*) FROM stated_goals WHERE cleared_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("count open");
        let total: i64 = connection
            .query_row("SELECT count(*) FROM stated_goals", [], |row| row.get(0))
            .expect("count all");

        assert_eq!(open, 1, "exactly one goal is current");
        assert_eq!(total, 2, "what someone worked towards before is not erased");
    }

    #[test]
    fn a_cleared_goal_returns_the_answer_to_inference() {
        let connection = Connection::open_in_memory().expect("memory database");
        connection
            .execute(
                r#"CREATE TABLE node_versions (
                       space_id TEXT, label TEXT, body TEXT, kind TEXT,
                       recorded_at INTEGER, superseded_at INTEGER
                   )"#,
                [],
            )
            .expect("schema");
        set(&connection, "default", "временная цель", 100).expect("set");

        let report = clear(&connection, "default", 200).expect("clear");
        assert_eq!(report["cleared"], 1);

        let after = goals(&connection, "default", 300).expect("goals");
        assert!(after["stated"].is_null(), "a withdrawn goal is not current");

        // Closed, not deleted: what someone worked towards stays in the record.
        let total: i64 = connection
            .query_row("SELECT count(*) FROM stated_goals", [], |row| row.get(0))
            .expect("count");
        assert_eq!(total, 1);
    }

    #[test]
    fn clearing_a_goal_nobody_set_says_so_rather_than_failing() {
        let connection = Connection::open_in_memory().expect("memory database");
        let report = clear(&connection, "default", 100).expect("clear");

        assert_eq!(report["status"], "ok");
        assert_eq!(report["cleared"], 0);
    }

    #[test]
    fn an_empty_goal_is_refused_rather_than_stored() {
        let connection = Connection::open_in_memory().expect("memory database");
        let report = set(&connection, "default", "   ", 100).expect("set");

        assert_eq!(report["status"], "error");
    }
}
