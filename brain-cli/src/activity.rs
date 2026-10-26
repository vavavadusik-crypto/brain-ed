//! Who is using the memory right now.
//!
//! The hook journal already records every agent event; nothing read it back.
//! The window could say how the engine was tuned and how much it held, but not
//! whether anything was alive — so a running system and a dead one looked
//! identical.
//!
//! What this deliberately does not surface: working directories, prompt text,
//! file names, memory contents. The agent's name and the rhythm of its calls
//! are enough to see life, and this screen gets shown to other people.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

/// Journal bytes read per refresh, from the end.
///
/// The file grows without bound — it passed 4.9 MB on the author's machine —
/// and the panel refreshes every few seconds. Reading a fixed tail keeps the
/// cost flat no matter how long the journal has been accumulating.
const TAIL_BYTES: u64 = 256 * 1024;

/// An agent seen this recently is working now.
const ACTIVE_WINDOW: i64 = 120;
/// Seen within this, it was here a moment ago.
const RECENT_WINDOW: i64 = 1_800;

/// One line of the hook journal.
///
/// Only these three fields are read. `payload` carries the working directory
/// and tool arguments, and is deliberately left out of the struct so it cannot
/// reach the page by accident.
#[derive(Deserialize)]
struct JournalEntry {
    event: String,
    client: String,
    received_at: i64,
}

/// What one agent has been doing.
struct AgentActivity {
    events: usize,
    last_seen: i64,
    last_event: String,
    tool_calls: usize,
    sessions_started: usize,
}

/// Reads the tail of the hook journal and summarises it per agent.
///
/// A missing or unreadable journal is not an error: it means no agent has
/// reported yet, which the page states plainly rather than showing nothing.
pub(crate) fn document(brain_home: &Path, now: i64) -> Value {
    let journal = brain_home.join("hook-events.jsonl");
    let Some(tail) = read_tail(&journal) else {
        return json!({
            "available": false,
            "reason": "журнал событий агентов пока пуст — ни один агент ещё не обращался",
            "agents": [],
        });
    };

    let mut agents: BTreeMap<String, AgentActivity> = BTreeMap::new();
    let mut parsed = 0usize;
    let mut invalid = 0usize;
    let mut future = 0usize;
    for line in tail.text.lines() {
        let Ok(entry) = serde_json::from_str::<JournalEntry>(line) else {
            invalid += 1;
            continue;
        };
        if entry.received_at > now {
            future += 1;
            continue;
        }
        parsed += 1;
        let activity = agents.entry(entry.client).or_insert_with(|| AgentActivity {
            events: 0,
            last_seen: 0,
            last_event: String::new(),
            tool_calls: 0,
            sessions_started: 0,
        });
        activity.events += 1;
        if entry.received_at >= activity.last_seen {
            activity.last_seen = entry.received_at;
            activity.last_event.clone_from(&entry.event);
        }
        match entry.event.as_str() {
            "PreToolUse" => activity.tool_calls += 1,
            "SessionStart" => activity.sessions_started += 1,
            _ => {}
        }
    }

    let mut listed = agents
        .into_iter()
        .map(|(name, activity)| {
            let idle = now.saturating_sub(activity.last_seen).max(0);
            json!({
                "agent": name,
                "state": state_for(idle),
                "idle_seconds": idle,
                "events": activity.events,
                "tool_calls": activity.tool_calls,
                "sessions_started": activity.sessions_started,
                "last_event": activity.last_event,
                "last_seen": activity.last_seen,
            })
        })
        .collect::<Vec<_>>();
    // Most recently seen first: whoever is working now belongs at the top.
    listed.sort_by_key(|entry| -entry["last_seen"].as_i64().unwrap_or(0));

    let working_now = listed
        .iter()
        .filter(|entry| entry["state"] == "работает")
        .count();

    json!({
        "available": true,
        "agents": listed,
        "working_now": working_now,
        "events_read": parsed,
        "invalid_records": invalid,
        "future_records": future,
        "tail_truncated": tail.truncated,
        "partial_last_record": tail.partial_last,
        "capture_kind": "event_metadata_only",
        "conversation_coverage": "not_implemented",
        "window_note": "по последним событиям журнала, не за всё время",
    })
}

/// Turns silence into one of three words a person can read at a glance.
fn state_for(idle_seconds: i64) -> &'static str {
    if idle_seconds <= ACTIVE_WINDOW {
        "работает"
    } else if idle_seconds <= RECENT_WINDOW {
        "недавно"
    } else {
        "спит"
    }
}

/// Reads the last `TAIL_BYTES` of a file, or the whole file when it is smaller.
///
/// Read at most a fixed-size snapshot, retaining every complete record. An
/// unfinished final line may be an in-flight append; report it separately.
struct JournalTail {
    text: String,
    truncated: bool,
    partial_last: bool,
}

fn read_tail(path: &Path) -> Option<JournalTail> {
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length == 0 {
        return None;
    }
    let start = length.saturating_sub(TAIL_BYTES);
    // Read one preceding byte to distinguish a record boundary from a cut.
    let read_start = start.saturating_sub(1);
    file.seek(SeekFrom::Start(read_start)).ok()?;
    let mut bytes = Vec::with_capacity(TAIL_BYTES as usize);
    file.take(length - read_start)
        .read_to_end(&mut bytes)
        .ok()?;
    let mut first = 0;
    if start > 0 {
        first = if bytes.first() == Some(&b'\n') {
            1
        } else {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |i| i + 1)
        };
    }
    let partial_last = bytes.last() != Some(&b'\n');
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |i| i + 1);
    let kept = &bytes[first.min(end)..end];
    // Lossy on purpose: a multi-byte character split by the tail boundary must
    // not discard the entire journal.
    Some(JournalTail {
        text: String::from_utf8_lossy(kept).into_owned(),
        truncated: start > 0,
        partial_last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Unique scratch directory, matching how the rest of this crate tests.
    fn scratch(_name: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let _nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = crate::test_support::temp_path("brain-activity");
        std::fs::create_dir_all(&root).expect("scratch directory");
        root
    }

    fn journal(name: &str, lines: &[&str]) -> PathBuf {
        let root = scratch(name);
        let mut file = File::create(root.join("hook-events.jsonl")).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        root
    }

    #[test]
    fn an_agent_that_just_called_is_shown_as_working() {
        let directory = journal(
            "working",
            &[
                r#"{"event":"SessionStart","client":"first","received_at":1000}"#,
                r#"{"event":"SessionStart","client":"claude","received_at":9000}"#,
                r#"{"event":"PreToolUse","client":"claude","received_at":9950}"#,
            ],
        );
        let document = document(&directory, 10_000);

        assert_eq!(document["available"], true);
        assert_eq!(document["working_now"], 1);
        let agents = document["agents"].as_array().expect("agents");
        assert_eq!(agents[0]["agent"], "claude");
        assert_eq!(agents[0]["state"], "работает");
        assert_eq!(agents[0]["tool_calls"], 1);
    }

    #[test]
    fn silence_separates_working_from_recent_from_asleep() {
        let directory = journal(
            "states",
            &[
                r#"{"event":"SessionStart","client":"ignored-first-line","received_at":1}"#,
                r#"{"event":"Stop","client":"now","received_at":9990}"#,
                r#"{"event":"Stop","client":"lately","received_at":9000}"#,
                r#"{"event":"Stop","client":"asleep","received_at":100}"#,
            ],
        );
        let document = document(&directory, 10_000);
        let agents = document["agents"].as_array().expect("agents");

        assert_eq!(agents[0]["state"], "работает");
        assert_eq!(agents[1]["state"], "недавно");
        assert_eq!(agents[2]["state"], "спит");
        assert_eq!(document["working_now"], 1);
    }

    /// The panel gets shown to other people, so what it omits is a feature.
    #[test]
    fn no_path_or_payload_reaches_the_page() {
        let directory = journal(
            "privacy",
            &[
                r#"{"event":"SessionStart","client":"skipped","received_at":1}"#,
                r#"{"event":"PreToolUse","client":"claude","received_at":9000,"payload":{"cwd":"/home/someone/private-project","tool_name":"Bash"}}"#,
            ],
        );
        let rendered = document(&directory, 9_100).to_string();

        assert!(!rendered.contains("private-project"), "path leaked");
        assert!(!rendered.contains("/home/"), "home directory leaked");
        assert!(!rendered.contains("payload"), "payload leaked");
    }

    #[test]
    fn a_missing_journal_says_so_instead_of_showing_nothing() {
        let directory = scratch("missing");
        let document = document(&directory, 10_000);

        assert_eq!(document["available"], false);
        assert!(
            document["reason"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "an unavailable panel must explain itself"
        );
    }

    #[test]
    fn one_complete_event_is_visible() {
        let directory = journal(
            "single",
            &[r#"{"event":"SessionStart","client":"claude","received_at":1000}"#],
        );
        let result = document(&directory, 1001);
        assert_eq!(result["events_read"], 1);
        assert_eq!(result["agents"][0]["agent"], "claude");
        assert_eq!(result["tail_truncated"], false);
    }

    #[test]
    fn damaged_future_and_partial_records_are_reported_not_live() {
        let directory = journal(
            "damage",
            &[
                "not json",
                r#"{"event":"SessionStart","client":"future","received_at":2000}"#,
            ],
        );
        let path = directory.join("hook-events.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"{\"event\":")
            .unwrap();
        let result = document(&directory, 1000);
        assert_eq!(result["invalid_records"], 1);
        assert_eq!(result["future_records"], 1);
        assert_eq!(result["partial_last_record"], true);
        assert_eq!(result["working_now"], 0);
    }

    #[test]
    fn tail_keeps_complete_first_line_at_exact_boundary() {
        let directory = scratch("boundary");
        let row = "{\"event\":\"Stop\",\"client\":\"claude\",\"received_at\":1000}";
        let mut bytes = b"discarded\n".to_vec();
        let padding = TAIL_BYTES as usize - row.len() - 2;
        bytes.extend_from_slice(row.as_bytes());
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(b' ', padding));
        bytes.push(b'\n');
        std::fs::write(directory.join("hook-events.jsonl"), bytes).unwrap();
        let result = document(&directory, 1000);
        assert_eq!(result["tail_truncated"], true);
        assert_eq!(result["events_read"], 1);
    }

    /// A tail cut mid-line must not take the surrounding entries with it.
    #[test]
    fn a_truncated_first_line_is_discarded_not_fatal() {
        let directory = journal(
            "torn",
            &[
                r#"nt":"PreToolUse","client":"torn","received_at":900}"#,
                r#"{"event":"PreToolUse","client":"claude","received_at":9000}"#,
            ],
        );
        let document = document(&directory, 9_100);

        let agents = document["agents"].as_array().expect("agents");
        assert_eq!(agents.len(), 1, "only the intact entry counts");
        assert_eq!(agents[0]["agent"], "claude");
    }
}
