//! `brain stats`: what the one brain holds, broken down by where it came from.
//!
//! `status` answers "is the database healthy"; this answers "is every source
//! actually in it, and how fresh is each one". Reads only: the database is
//! opened read-only and the archive through its own status document.

use std::error::Error;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

/// Current, still-valid assertions grouped by origin. Markdown imports are
/// split by the first `--tag` the import was run with.
const SOURCES_SQL: &str = "
    SELECT CASE
             -- Graphify nodes keep their `g:` identity whichever agent imported them.
             WHEN node_id LIKE 'g:%' THEN 'graphify'
             WHEN json_extract(metadata, '$.imported_from') = 'markdown'
               THEN 'markdown:' || COALESCE(json_extract(metadata, '$.import_tags[0]'), 'untagged')
             WHEN json_extract(metadata, '$.imported_from') IS NOT NULL
               THEN json_extract(metadata, '$.imported_from')
             ELSE 'agent:' || agent_id
           END AS origin,
           count(*),
           max(valid_from),
           max(recorded_at)
    FROM node_versions
    -- The same two time axes as `brain status` (storage status(), active_nodes).
    WHERE space_id = ?1
      AND valid_from <= ?2 AND (valid_to IS NULL OR ?2 < valid_to)
      AND recorded_at <= ?2 AND (superseded_at IS NULL OR ?2 < superseded_at)
    GROUP BY origin
    ORDER BY count(*) DESC, origin";

pub(crate) fn document(
    database: &Path,
    space: &str,
    archive_status: &Value,
    now: i64,
) -> Result<Value, Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| format!("{}: {error}", database.display()))?;
    let mut statement = connection.prepare(SOURCES_SQL)?;
    let rows = statement.query_map(params![space, now], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut sources = Vec::new();
    let mut total = 0_i64;
    for row in rows {
        let (origin, nodes, latest_event, latest_recorded) = row?;
        total += nodes;
        sources.push(json!({
            "source": origin,
            "nodes": nodes,
            "latest_event_at": latest_event,
            "latest_recorded_at": latest_recorded,
        }));
    }
    // Stored edge identities (history included, as in `status.edges`) and the
    // edges valid now, counted on the same two time axes as the nodes above.
    let (edges, active_edges): (i64, i64) = connection.query_row(
        "SELECT (SELECT count(*) FROM edges WHERE space_id = ?1),
                -- Version rows, joined as in `status`, so the two counts stay equal
                -- even if overlapping revisions of one edge are both valid now.
                (SELECT count(*) FROM edge_versions AS v
                 JOIN edges AS e ON e.id = v.edge_id AND e.space_id = v.space_id
                 WHERE v.space_id = ?1
                   AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
                   AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at))",
        params![space, now],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut document = json!({
        "status": "ok",
        "database": database,
        "space": space,
        "nodes": total,
        "edges": edges,
        "active_edges": active_edges,
        "sources": sources,
        // The archive is a separate private store; its own counts are reported
        // as they are, including entries quarantined out of search.
        "conversation_archive": archive_status.get("stored").cloned().unwrap_or(Value::Null),
    });
    add_date_siblings(&mut document);
    Ok(document)
}

/// Gives every `*_at` field holding unix seconds a `*_date` sibling in
/// `YYYY-MM-DD HH:MM` UTC, anywhere in the document.
///
/// The numeric fields stay: scripts compare them, humans read the date. Fields
/// whose name ends in `_at` but that do not hold unix seconds — the archive's
/// ISO-8601 strings — are left alone: they already name a date.
fn add_date_siblings(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            let mut siblings = Vec::new();
            for (key, field) in fields.iter_mut() {
                add_date_siblings(field);
                if let Some(seconds) = field.as_i64()
                    && key.ends_with("_at")
                {
                    siblings.push((
                        format!("{}_date", &key[..key.len() - "_at".len()]),
                        json!(utc_minute(seconds)),
                    ));
                }
            }
            for (key, date) in siblings {
                fields.insert(key, date);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(add_date_siblings),
        _ => {}
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` in UTC for a unix-seconds timestamp.
pub(crate) fn utc_second(seconds: i64) -> String {
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        time_of_day % 3_600 / 60,
        time_of_day % 60
    )
}

/// Wall-clock `YYYY-MM-DD HH:MM` in UTC for a unix-seconds timestamp.
pub(crate) fn utc_minute(seconds: i64) -> String {
    let day = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, date) = civil_from_days(day);
    format!(
        "{year:04}-{month:02}-{date:02} {:02}:{:02}",
        time_of_day / 3_600,
        time_of_day % 3_600 / 60
    )
}

/// Inverse of days-from-civil (Howard Hinnant): the calendar date for a count
/// of days since 1970-01-01, valid for any i64 the seconds can produce.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        date as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_database_is_an_error_not_an_empty_brain() {
        let dir = crate::test_support::temp_path("brain-stats");
        let error = document(&dir.join("absent.sqlite"), "default", &Value::Null, 0)
            .expect_err("missing database");
        assert!(error.to_string().contains("absent.sqlite"));
    }

    #[test]
    fn utc_second_formats_known_timestamps() {
        assert_eq!(utc_second(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_second(1_783_548_804), "2026-07-08T22:13:24Z");
        assert_eq!(utc_second(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(utc_second(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn utc_minute_formats_known_timestamps() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00");
        assert_eq!(utc_minute(1_500_000_000), "2017-07-14 02:40");
        assert_eq!(utc_minute(1_709_164_800), "2024-02-29 00:00");
        assert_eq!(utc_minute(-60), "1969-12-31 23:59");
    }

    #[test]
    fn date_siblings_follow_numeric_fields_anywhere_but_skip_strings() {
        let mut document = json!({
            "kept_at": 1_500_000_000,
            "nested": [{"also_at": 0, "label": "x"}],
            "iso_at": "2017-07-14T02:40:00Z",
            "already_dated_at": 86_399,
            "not_a_time": 5,
        });
        add_date_siblings(&mut document);
        assert_eq!(document["kept_date"], "2017-07-14 02:40");
        assert_eq!(document["nested"][0]["also_date"], "1970-01-01 00:00");
        assert_eq!(document["already_dated_date"], "1970-01-01 23:59");
        // ISO-8601 strings and non-_at numbers are untouched, and no
        // double-prefixing happens when the pair already exists.
        assert!(document["iso_at"].is_string());
        assert!(document.get("iso_date").is_none());
        assert!(document.get("not_a_time_date").is_none());
        assert_eq!(document["already_dated_at"], 86_399);
    }
}
