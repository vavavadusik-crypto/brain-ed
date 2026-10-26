//! `brain archive export --format otlp-json`: the conversation journal speaks
//! OpenTelemetry GenAI.
//!
//! One session becomes one trace; every stored entry becomes a span in that
//! trace carrying `gen_ai.*` attributes per the GenAI semantic conventions
//! (Development status for most `gen_ai.*` attributes; `error.type` is
//! Stable). Time comes from `said_at` (the readable UTC form of the client's
//! timestamp). Content is the already-masked archive text; `--no-content`
//! keeps metadata only. Quarantined rows are never exported. Nothing here
//! touches the network: the document is printed to stdout.

use std::error::Error;
use std::path::Path;

use rusqlite::params;
use serde_json::{Value, json};

use crate::installer::sha256;

/// Roles whose spans describe an agent's tool execution.
const TOOL_ROLES: [&str; 3] = ["tool_call", "tool_result", "tool_error"];

/// The GenAI provider name Brain ED reports: the client whose conversation it
/// is (claude/codex/hermes), lowercased as the conventions expect.
fn provider_name(client: &str) -> String {
    client.to_lowercase()
}

/// The operation for a role: `execute_tool` for tool rows, `chat` otherwise.
fn operation_name(role: &str) -> &'static str {
    if TOOL_ROLES.contains(&role) {
        "execute_tool"
    } else {
        "chat"
    }
}

/// `traceId`/`spanId` are hex strings in OTLP/JSON: derived deterministically
/// from the session and the entry id, so the same session re-exports with the
/// same identifiers.
fn trace_id(session: &str) -> String {
    sha256(session.as_bytes())[..32].to_owned()
}

fn span_id(session: &str, entry_id: i64) -> String {
    sha256(format!("{session}:{entry_id}").as_bytes())[..16].to_owned()
}

fn attr(key: &str, value: impl Into<Value>) -> Value {
    json!({"key": key, "value": {"stringValue": value.into()}})
}

fn attr_bool(key: &str, value: bool) -> Value {
    json!({"key": key, "value": {"boolValue": value}})
}

fn attr_int(key: &str, value: i64) -> Value {
    json!({"key": key, "value": {"intValue": value}})
}

fn unix_nanos(said_at: Option<f64>) -> String {
    said_at
        .map(|seconds| format!("{}", (seconds * 1_000_000_000.0) as i128))
        .unwrap_or_else(|| "0".to_owned())
}

/// One row of the archive journal.
type EntryRow = (i64, String, String, Option<String>, Option<f64>, String);

/// One span per archive row, with the role-mapped `gen_ai.*` attributes.
fn entry_span(session: &str, row: &EntryRow, no_content: bool) -> Value {
    let (id, client, role, _session_id, said_at, content) = row;
    let operation = operation_name(role);
    let mut attributes = vec![
        attr("gen_ai.provider.name", provider_name(client)),
        attr("gen_ai.operation.name", operation),
        attr("gen_ai.conversation.id", session),
        attr("gen_ai.tool.name", format!("brain.archive.{role}")),
        attr("brain.archive.role", role.as_str()),
        attr_int("brain.archive.entry_id", *id),
    ];
    if no_content {
        attributes.push(attr_bool("brain.archive.content_omitted", true));
    } else {
        attributes.push(attr("brain.archive.content", content.as_str()));
    }
    // The archive has no recorded model; retain an explicit conversation label.
    attributes.push(attr(
        "gen_ai.request.model",
        format!("{} conversation", provider_name(client)),
    ));
    if !no_content {
        let message_role = match role.as_str() {
            "user" => "user",
            "assistant" | "reasoning" => "assistant",
            _ => "tool",
        };
        let key = if role == "user" {
            "gen_ai.input.messages"
        } else {
            "gen_ai.output.messages"
        };
        attributes.push(attr(
            key,
            json!([{
                "role": message_role,
                "parts": [{"type": "text", "content": content}],
            }])
            .to_string(),
        ));
    }
    if role == "tool_error" {
        // `error.type` is the one Stable attribute this export relies on.
        attributes.push(attr("error.type", "tool_error"));
    }
    let span_name = if operation == "execute_tool" {
        format!("{operation} brain.archive.{role}")
    } else {
        // Span name SHOULD be `{operation.name} {request.model}`; Brain ED's
        // journal has no model per row, so the provider takes that slot.
        format!("{operation} {}", provider_name(client))
    };
    json!({
        "traceId": trace_id(session),
        "spanId": span_id(session, *id),
        "name": span_name,
        "kind": 1,
        "startTimeUnixNano": unix_nanos(*said_at),
        "endTimeUnixNano": unix_nanos(*said_at),
        "attributes": attributes,
        "status": if role == "tool_error" {
            json!({"code": 2})
        } else {
            json!({"code": 0})
        },
    })
}

/// A session trace with its conversation attributes.
fn session_trace(session: &str, rows: &[EntryRow], no_content: bool) -> Value {
    let provider = rows
        .first()
        .map(|(_, client, _, _, _, _)| provider_name(client))
        .unwrap_or_default();
    let spans: Vec<Value> = rows
        .iter()
        .map(|row| entry_span(session, row, no_content))
        .collect();
    json!({
        "resource": {
            "attributes": [
                attr("service.name", "brain-ed"),
                attr("gen_ai.provider.name", provider),
            ],
        },
        "scopeSpans": [{
            "scope": {"name": "brain-ed.archive", "version": env!("CARGO_PKG_VERSION")},
            "spans": spans,
        }],
        "schemaUrl": "https://opentelemetry.io/schemas/1.44.0",
    })
}

/// `brain archive export --format otlp-json`: the whole OTLP/JSON document.
///
/// Quarantined rows never match (`visible`), the date window filters on
/// `said_at` (the readable UTC form of the client timestamp), and the content
/// column is dropped entirely for `--no-content`.
pub(crate) fn export_otlp_json(
    brain_home: &Path,
    session: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
    no_content: bool,
) -> Result<Value, Box<dyn Error>> {
    let database = brain_home.join("archive/sessions.sqlite");
    let archive = crate::archive::open(&database)?;
    let connection = &archive.connection;
    let visible = archive.visible("e");
    // Claude and Codex record ISO-8601 times, Hermes Unix seconds as text.
    let said_at = "(CASE WHEN e.ts GLOB '[0-9]*' AND e.ts NOT GLOB '*[^0-9.]*' \
                   THEN CAST(e.ts AS REAL) ELSE CAST(strftime('%s', e.ts) AS REAL) END)";
    // `--no-content` must not read the column at all, not just drop it from the output.
    let content = if no_content { "''" } else { "e.content" };
    let sql = format!(
        "SELECT e.id, s.client, e.role, e.session_id, {said_at}, {content}
         FROM entries e JOIN sources s ON s.id = e.source_id
         WHERE {visible}
           AND (?1 IS NULL OR e.session_id = ?1)
           AND (?2 IS NULL OR {said_at} >= ?2)
           AND (?3 IS NULL OR {said_at} < ?3)
         ORDER BY COALESCE(e.session_id, ''), e.id"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(params![session, since, until], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<f64>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Group by session id; rows without a session land in a synthetic group.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::BTreeMap<String, Vec<_>> = Default::default();
    for row in rows {
        let session = row
            .3
            .clone()
            .unwrap_or_else(|| format!("no-session:{}", row.0));
        if !groups.contains_key(&session) {
            order.push(session.clone());
        }
        groups.entry(session.clone()).or_default().push(row);
    }
    let traces: Vec<Value> = order
        .iter()
        .filter_map(|session| {
            groups
                .remove(session)
                .map(|rows| session_trace(session, &rows, no_content))
        })
        .collect();
    Ok(json!({
        "resourceSpans": traces,
        "export_note": "OTLP/JSON prototype format; printed locally, nothing is sent over the network",
    }))
}
