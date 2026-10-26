//! M2 ingestion. The coordinator's event log becomes timeline events, and,
//! only when explicitly asked, archive session *metadata* becomes
//! `has_session` proposals for projects whose declared roots contain a
//! session's working directory.
//!
//! Sources are opened read-only after ownership, mode and symlink checks and
//! read inside one snapshot transaction. A batch's events, its seen-keys and
//! its cursor commit in one map transaction, so an interrupted batch leaves
//! nothing behind and a repeated one adds nothing. No message text, note or
//! data blob is ever selected from a source.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use serde_json::{Value, json};

use super::events::{
    Authority, EventDraft, LocalUserGrant, MAX_EVIDENCE, validate_ref, validate_space,
};
use super::store::{COLUMNS, MapStore, Writer, page_limit, read_event, sql_limit};
use super::{MapError, MapErrorKind};

/// Version of the ingest bookkeeping (cursors, seen-keys) and source mapping.
pub const INGEST_FORMAT: &str = "brain-ingest/1";
/// "BRNC", the coordinator's `application_id`.
pub const COORD_APPLICATION_ID: i64 = 0x4252_4E43;
pub const COORD_SCHEMA_VERSION: i64 = 1;
/// `archive_meta.schema` of the only archive layout read.
pub const ARCHIVE_SCHEMA: &str = "brain_session_archive:3";
/// Longest wait for a source writer's lock.
const SOURCE_BUSY: Duration = Duration::from_millis(500);
/// A batch that takes longer is abandoned whole.
const MAX_RUNTIME: Duration = Duration::from_secs(10);
const MAX_SOURCE_PATH: usize = 1_024;
const MAX_TOKEN: usize = 64;
const MAX_COORD_KIND: usize = 32;
const MAX_SESSION: usize = 128;
const MAX_CWD: usize = 4_096;

const COORD_TABLES: [&str; 6] = ["meta", "tasks", "task_deps", "claims", "events", "requests"];
const COORD_EVENTS: [(&str, &str); 11] = [
    ("id", "INTEGER"),
    ("space", "TEXT"),
    ("task_id", "INTEGER"),
    ("claim_id", "INTEGER"),
    ("kind", "TEXT"),
    ("agent", "TEXT"),
    ("machine", "TEXT"),
    ("fence", "INTEGER"),
    ("at", "INTEGER"),
    ("note", "TEXT"),
    ("data", "TEXT"),
];
const ARCHIVE_TABLES: [&str; 4] = ["archive_meta", "sources", "entries", "cursors"];
const ARCHIVE_SOURCES: [(&str, &str); 4] = [
    ("id", "INTEGER"),
    ("client", "TEXT"),
    ("path", "TEXT"),
    ("path_sha256", "TEXT"),
];
const ARCHIVE_ENTRIES: [(&str, &str); 13] = [
    ("id", "INTEGER"),
    ("source_id", "INTEGER"),
    ("session_id", "TEXT"),
    ("role", "TEXT"),
    ("locator", "TEXT"),
    ("source_offset", "INTEGER"),
    ("ts", "TEXT"),
    ("content", "TEXT"),
    ("content_sha256", "TEXT"),
    ("evidence_sha256", "TEXT"),
    ("record_sha256", "TEXT"),
    ("imported_at", "TEXT"),
    ("cwd", "TEXT"),
];
/// Never `note` or `data`: those are agent text. `substr` bounds what a row can load.
const COORD_SELECT: &str = "SELECT id, task_id, claim_id, substr(kind, 1, 33), substr(agent, 1, 65), \
     substr(machine, 1, 65), fence, at FROM events WHERE space = ?1";
/// Never `content`.
const ARCHIVE_SELECT: &str = "SELECT e.id, substr(s.client, 1, 65), substr(e.session_id, 1, 129), \
     substr(e.cwd, 1, 4097), e.record_sha256 FROM entries e JOIN sources s ON s.id = e.source_id WHERE 1 = 1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Coordinator,
    Archive,
}

impl SourceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Archive => "archive",
        }
    }
}

/// What one batch did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IngestReport {
    /// Source rows read in this batch.
    pub read: usize,
    pub appended: usize,
    /// Rows or links already ingested with identical content (after an explicit reset).
    pub already_seen: usize,
    /// Archive rows with no usable session, client or cwd, or matching no declared root.
    pub unmatched: usize,
    /// Source row id the cursor now stands on.
    pub position: i64,
    /// No further rows were waiting when the batch was read.
    pub caught_up: bool,
}

/// One timeline item, read straight from the ledger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineEntry {
    pub event_id: String,
    pub local_seq: i64,
    pub subject: String,
    /// `coord_recorded`, `outcome_claimed` or `outcome_observed`.
    pub kind: String,
    /// The coordinator's own event word (`created`, `claimed`, `completed`, …); empty otherwise.
    pub coord_kind: String,
    /// `claimed_completed`, `claimed_failed`, `unknown_lease_expired`, … or empty.
    pub outcome: String,
    pub basis: String,
    pub agent: String,
    pub actor_machine: String,
    pub fence: Option<i64>,
    pub claim_id: Option<i64>,
    /// The source's clock (coordinator), never compared with the map's clocks.
    pub source_at: Option<i64>,
    pub recorded_at: i64,
    pub received_at: i64,
    pub evidence: Vec<String>,
}

/// Which file a cursor belongs to: device and inode, plus the path it was read at.
#[derive(Clone)]
struct SourceId {
    identity: String,
    path: String,
}

struct Source {
    conn: Connection,
    id: SourceId,
}

fn current_uid() -> Result<u32, MapError> {
    Ok(fs::metadata("/proc/self")?.uid())
}

fn check_owned(path: &Path, what: &str, dir: bool) -> Result<fs::Metadata, MapError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(MapError::new(
                MapErrorKind::NotFound,
                format!("{what} does not exist"),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let refuse = |why: &str| MapError::new(MapErrorKind::Incompatible, format!("{what} {why}"));
    if meta.file_type().is_symlink() {
        return Err(refuse("is a symlink"));
    }
    if (dir && !meta.is_dir()) || (!dir && !meta.is_file()) {
        return Err(refuse(if dir {
            "is not a directory"
        } else {
            "is not a regular file"
        }));
    }
    if meta.uid() != current_uid()? || meta.mode() & 0o077 != 0 {
        return Err(refuse(
            "must be owned by this user and closed to group and other",
        ));
    }
    Ok(meta)
}

/// Opens an explicit source path read-only after the safety checks.
fn open_source(path: &Path) -> Result<Source, MapError> {
    let text = path
        .to_str()
        .ok_or_else(|| MapError::invalid("source path is not UTF-8"))?;
    if !path.is_absolute()
        || text.len() > MAX_SOURCE_PATH
        || text.chars().any(char::is_control)
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(MapError::invalid(
            "source path must be absolute, normalised and at most 1024 bytes",
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| MapError::invalid("source path has no directory"))?;
    check_owned(dir, "source directory", true)?;
    let meta = check_owned(path, "source", false)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = PathBuf::from(format!("{text}{suffix}"));
        if fs::symlink_metadata(&side).is_ok() {
            check_owned(&side, "source side file", false)?;
        }
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(SOURCE_BUSY)?;
    conn.pragma_update(None, "query_only", true)?;
    let again = fs::symlink_metadata(path)?;
    if (again.dev(), again.ino()) != (meta.dev(), meta.ino()) {
        return Err(MapError::new(
            MapErrorKind::Conflict,
            "source changed while it was being opened",
        ));
    }
    Ok(Source {
        conn,
        id: SourceId {
            identity: format!("{}:{}", meta.dev(), meta.ino()),
            path: text.to_owned(),
        },
    })
}

fn incompatible(message: impl Into<String>) -> MapError {
    MapError::new(MapErrorKind::Incompatible, message)
}

fn require_tables(c: &Connection, tables: &[&str]) -> Result<(), MapError> {
    for table in tables {
        let present: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |r| r.get(0),
        )?;
        if !present {
            return Err(incompatible(format!("source lacks table {table}")));
        }
    }
    Ok(())
}

/// Exact column names, types and order: an unknown layout is refused, not guessed.
fn require_shape(c: &Connection, table: &str, expected: &[(&str, &str)]) -> Result<(), MapError> {
    let mut stmt = c.prepare("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")?;
    let found: Vec<(String, String)> = stmt
        .query_map([table], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let same = found.len() == expected.len()
        && found
            .iter()
            .zip(expected)
            .all(|((n, t), (en, et))| n == en && t == et);
    if same {
        Ok(())
    } else {
        Err(incompatible(format!(
            "source table {table} has an unknown column layout"
        )))
    }
}

fn pragma(c: &Connection, name: &str) -> Result<i64, MapError> {
    Ok(c.pragma_query_value(None, name, |r| r.get(0))?)
}

fn check_coordinator(c: &Connection) -> Result<(), MapError> {
    let app = pragma(c, "application_id")?;
    if app != COORD_APPLICATION_ID {
        return Err(incompatible(format!(
            "not a Brain coordination database (application_id {app:#x})"
        )));
    }
    let version = pragma(c, "user_version")?;
    if version != COORD_SCHEMA_VERSION {
        return Err(incompatible(format!(
            "coordination schema {version} is not {COORD_SCHEMA_VERSION}; refusing to guess"
        )));
    }
    require_tables(c, &COORD_TABLES)?;
    require_shape(c, "events", &COORD_EVENTS)
}

fn check_archive(c: &Connection) -> Result<(), MapError> {
    if pragma(c, "application_id")? != 0 || pragma(c, "user_version")? != 0 {
        return Err(incompatible(
            "not a Brain session archive (unexpected application_id or user_version)",
        ));
    }
    require_tables(c, &ARCHIVE_TABLES)?;
    let schema: Option<String> = c
        .query_row(
            "SELECT value FROM archive_meta WHERE key = 'schema'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if schema.as_deref() != Some(ARCHIVE_SCHEMA) {
        return Err(incompatible(format!(
            "archive schema is not {ARCHIVE_SCHEMA}; refusing to guess"
        )));
    }
    require_shape(c, "sources", &ARCHIVE_SOURCES)?;
    require_shape(c, "entries", &ARCHIVE_ENTRIES)
}

/// Length-prefixed BLAKE3 over fields, so boundaries are unambiguous.
fn fields_hash(parts: &[&str]) -> String {
    let mut h = blake3::Hasher::new();
    for part in parts {
        h.update(format!("{}:", part.len()).as_bytes());
        h.update(part.as_bytes());
        h.update(b"\n");
    }
    h.finalize().to_hex().to_string()
}

fn opt(v: Option<i64>) -> String {
    v.map_or_else(|| "none".to_owned(), |v| v.to_string())
}

fn token(name: &str, value: &str, max: usize) -> Result<(), MapError> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(MapError::invalid(format!(
            "source {name} is empty, over {max} bytes or has control characters"
        )));
    }
    Ok(())
}

struct Cursor {
    position: i64,
    last_hash: String,
}

/// The cursor is keyed by the source file's identity, not its path. A different
/// file at a known path is refused until the user resets that cursor.
fn cursor(
    w: &Writer<'_>,
    kind: SourceKind,
    source: &SourceId,
    space: &str,
) -> Result<Cursor, MapError> {
    let held = w
        .tx
        .query_row(
            "SELECT position, last_hash FROM ingest_cursors WHERE source_kind = ?1 AND identity = ?2 AND space = ?3",
            params![kind.as_str(), source.identity, space],
            |r| Ok(Cursor { position: r.get(0)?, last_hash: r.get(1)? }),
        )
        .optional()?;
    if let Some(held) = held {
        return Ok(held);
    }
    let replaced: bool = w.tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ingest_cursors WHERE source_kind = ?1 AND path = ?2 AND space = ?3 AND identity <> ?4)",
        params![kind.as_str(), source.path, space, source.identity],
        |r| r.get(0),
    )?;
    if replaced {
        return Err(MapError::new(
            MapErrorKind::Conflict,
            "a different file now sits at this source path; reset its cursor explicitly to read it",
        ));
    }
    Ok(Cursor {
        position: 0,
        last_hash: String::new(),
    })
}

fn save_cursor(
    w: &Writer<'_>,
    kind: SourceKind,
    source: &SourceId,
    space: &str,
    position: i64,
    hash: &str,
) -> Result<(), MapError> {
    let now = w.now()?;
    w.tx.execute(
        "INSERT INTO ingest_cursors(source_kind, identity, space, path, position, last_hash, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(source_kind, identity, space) DO UPDATE SET \
         path = excluded.path, position = excluded.position, last_hash = excluded.last_hash, updated_at = excluded.updated_at",
        params![kind.as_str(), source.identity, space, source.path, position, hash, now],
    )?;
    Ok(())
}

fn seen(w: &Writer<'_>, kind: SourceKind, space: &str, key: &str) -> Result<bool, MapError> {
    Ok(w.tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM ingest_seen WHERE source_kind = ?1 AND space = ?2 AND key = ?3)",
        params![kind.as_str(), space, key],
        |r| r.get(0),
    )?)
}

fn mark(
    w: &Writer<'_>,
    kind: SourceKind,
    space: &str,
    key: &str,
    event_id: &str,
) -> Result<(), MapError> {
    w.tx.execute(
        "INSERT INTO ingest_seen(source_kind, space, key, event_id) VALUES (?1, ?2, ?3, ?4)",
        params![kind.as_str(), space, key, event_id],
    )?;
    Ok(())
}

struct CoordRow {
    id: i64,
    task_id: Option<i64>,
    claim_id: Option<i64>,
    kind: String,
    agent: String,
    machine: String,
    fence: Option<i64>,
    at: i64,
}

fn read_coord(r: &Row<'_>) -> rusqlite::Result<CoordRow> {
    Ok(CoordRow {
        id: r.get(0)?,
        task_id: r.get(1)?,
        claim_id: r.get(2)?,
        kind: r.get(3)?,
        agent: r.get(4)?,
        machine: r.get(5)?,
        fence: r.get(6)?,
        at: r.get(7)?,
    })
}

impl CoordRow {
    fn hash(&self, space: &str) -> String {
        fields_hash(&[
            "coord-event/1",
            space,
            &self.id.to_string(),
            &opt(self.task_id),
            &opt(self.claim_id),
            &self.kind,
            &self.agent,
            &self.machine,
            &opt(self.fence),
            &self.at.to_string(),
        ])
    }

    fn validate(&self) -> Result<(), MapError> {
        token("kind", &self.kind, MAX_COORD_KIND)?;
        if !self
            .kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err(MapError::invalid(format!(
                "source event {} has an unknown kind shape",
                self.id
            )));
        }
        token("agent", &self.agent, MAX_TOKEN)?;
        token("machine", &self.machine, MAX_TOKEN)
    }

    /// `completed`/`failed` are what the claim holder *said*; an expired lease
    /// is an unknown outcome. Nothing here is an observation.
    fn draft(&self, space: &str) -> EventDraft {
        let outcome = match (self.kind.as_str(), self.task_id) {
            ("completed", Some(_)) => Some("claimed_completed"),
            ("failed", Some(_)) => Some("claimed_failed"),
            ("lease_expired", Some(_)) => Some("unknown_lease_expired"),
            _ => None,
        };
        let mut payload = json!({
            "coord_kind": self.kind, "coord_id": self.id, "source_at": self.at, "actor_machine": self.machine,
        });
        if let Some(fence) = self.fence {
            payload["fence"] = json!(fence);
        }
        if let Some(claim) = self.claim_id {
            payload["claim_id"] = json!(claim);
        }
        EventDraft {
            kind: if outcome.is_some() {
                "outcome_claimed"
            } else {
                "coord_recorded"
            }
            .into(),
            space: space.into(),
            subject: self.task_id.map_or_else(
                || format!("coord-event:{space}/{}", self.id),
                |task| format!("task:{space}/{task}"),
            ),
            relation: if outcome.is_some() { "outcome" } else { "" }.into(),
            object: outcome.map_or_else(String::new, |o| format!("outcome:{space}/{o}")),
            basis: "coordinator_record".into(),
            asserted_kind: "coordinator".into(),
            client: "coordinator".into(),
            agent: self.agent.clone(),
            evidence: vec![format!("coord-event:{space}/{}", self.id)],
            occurred_at: Some(self.at),
            occurred_at_source: "coordinator_clock".into(),
            confidence_milli: 900,
            corrects: None,
            payload: payload.to_string(),
        }
    }
}

struct EntryRow {
    id: i64,
    client: String,
    session: Option<String>,
    cwd: Option<String>,
    record: String,
}

fn read_entry(r: &Row<'_>) -> rusqlite::Result<EntryRow> {
    Ok(EntryRow {
        id: r.get(0)?,
        client: r.get(1)?,
        session: r.get(2)?,
        cwd: r.get(3)?,
        record: r.get(4)?,
    })
}

impl EntryRow {
    fn hash(&self) -> String {
        fields_hash(&[
            "archive-entry/1",
            &self.id.to_string(),
            &self.client,
            self.session.as_deref().unwrap_or("\u{0}"),
            self.cwd.as_deref().unwrap_or("\u{0}"),
            &self.record,
        ])
    }

    /// Client, session and a clean absolute cwd, or nothing to match.
    fn usable(&self) -> Option<(&str, &str, &Path)> {
        let (session, cwd) = (self.session.as_deref()?, self.cwd.as_deref()?);
        token("client", &self.client, MAX_TOKEN).ok()?;
        token("session", session, MAX_SESSION).ok()?;
        let path = Path::new(cwd);
        let clean = cwd.len() <= MAX_CWD
            && path.is_absolute()
            && !cwd.chars().any(char::is_control)
            && !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir));
        clean.then_some((self.client.as_str(), session, path))
    }
}

/// A root a project declared through a live `declared_root` link with evidence.
struct Root {
    project: String,
    path: PathBuf,
    evidence: Vec<String>,
}

fn declared_roots(w: &Writer<'_>, space: &str) -> Result<Vec<Root>, MapError> {
    let mut stmt = w.tx.prepare(
        "SELECT subject, object, current_events FROM links WHERE space = ?1 AND relation = 'declared_root' \
         AND status IN ('proposed', 'confirmed') ORDER BY subject, object",
    )?;
    let links: Vec<(String, String, String)> = stmt
        .query_map([space], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut roots = Vec::new();
    for (project, object, current) in links {
        // `project:<scope>/<absolute path>`
        let Some(path) = object
            .split_once(':')
            .and_then(|(_, rest)| rest.split_once('/'))
            .map(|(_, p)| PathBuf::from(p))
        else {
            continue;
        };
        if !path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            continue;
        }
        let mut evidence = Vec::new();
        for id in serde_json::from_str::<Vec<String>>(&current).unwrap_or_default() {
            let refs: Option<String> =
                w.tx.query_row(
                    "SELECT evidence FROM events WHERE event_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .optional()?;
            evidence.extend(
                serde_json::from_str::<Vec<String>>(&refs.unwrap_or_default()).unwrap_or_default(),
            );
        }
        evidence.sort();
        evidence.dedup();
        if !evidence.is_empty() {
            roots.push(Root {
                project,
                path,
                evidence,
            });
        }
    }
    Ok(roots)
}

/// Work one ingest batch may do. SQLite's progress handler runs every
/// [`PROGRESS_OPS`] virtual-machine steps on **both** the source and the map
/// connection and interrupts the running statement (prepare, query, collect,
/// root lookup, projection) once either limit is passed. This bounds SQL CPU
/// work cooperatively; it is not a hard real-time or I/O guarantee (a blocked
/// read or a busy wait ≤500 ms is not interrupted).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IngestBudget {
    pub max_runtime: Duration,
    /// Progress-handler calls allowed, each about `PROGRESS_OPS` VM steps.
    pub max_vm_steps: u64,
}

impl Default for IngestBudget {
    fn default() -> Self {
        Self {
            max_runtime: MAX_RUNTIME,
            max_vm_steps: u64::MAX,
        }
    }
}

pub const PROGRESS_OPS: i32 = 64;

/// Shared by both connections of a batch.
#[derive(Clone)]
struct Meter {
    deadline: Instant,
    max: u64,
    steps: Arc<AtomicU64>,
    tripped: Arc<AtomicBool>,
}

impl Meter {
    fn new(budget: IngestBudget) -> Self {
        Self {
            deadline: Instant::now() + budget.max_runtime,
            max: budget.max_vm_steps,
            steps: Arc::new(AtomicU64::new(0)),
            tripped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn attach(&self, c: &Connection) -> Result<(), MapError> {
        let m = self.clone();
        c.progress_handler(
            PROGRESS_OPS,
            Some(move || {
                // Interrupt once; the rollback that follows must be allowed to run.
                if m.tripped.load(Ordering::SeqCst) {
                    return false;
                }
                let n = m.steps.fetch_add(1, Ordering::SeqCst) + 1;
                let stop = n > m.max || Instant::now() > m.deadline;
                if stop {
                    m.tripped.store(true, Ordering::SeqCst);
                }
                stop
            }),
        )?;
        Ok(())
    }

    fn detach(c: &Connection) {
        let _ = c.progress_handler(0, None::<fn() -> bool>);
    }

    fn passed(&self) -> Result<(), MapError> {
        if self.tripped.load(Ordering::SeqCst) || Instant::now() > self.deadline {
            self.tripped.store(true, Ordering::SeqCst);
            return Err(budget_error());
        }
        Ok(())
    }

    /// An interrupted statement surfaces as a storage error; name the real cause.
    fn result<T>(&self, out: Result<T, MapError>) -> Result<T, MapError> {
        match out {
            Err(_) if self.tripped.load(Ordering::SeqCst) => Err(budget_error()),
            other => other,
        }
    }
}

fn budget_error() -> MapError {
    MapError::new(
        MapErrorKind::Unavailable,
        "ingest stopped by its budget (runtime or SQL steps); nothing from this batch was kept",
    )
}

/// An archive the trusted host configured, with its durable installation id.
/// The id is portable across machines, unlike device/inode; it says which
/// archive a ref points into, not who wrote the entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveSource<'a> {
    pub install: &'a str,
    pub path: &'a Path,
}

/// `[a-z0-9][a-z0-9._-]{0,63}`.
pub fn validate_install(install: &str) -> Result<(), MapError> {
    let ok = !install.is_empty()
        && install.len() <= MAX_TOKEN
        && install
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && install
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(MapError::invalid(
            "install id must match [a-z0-9][a-z0-9._-]{0,63}",
        ))
    }
}

/// One install id names one archive path, and one path one install id.
fn bind_install(w: &Writer<'_>, install: &str, path: &str) -> Result<(), MapError> {
    let by_install: Option<String> =
        w.tx.query_row(
            "SELECT path FROM archive_installs WHERE install = ?1",
            [install],
            |r| r.get(0),
        )
        .optional()?;
    let by_path: Option<String> =
        w.tx.query_row(
            "SELECT install FROM archive_installs WHERE path = ?1",
            [path],
            |r| r.get(0),
        )
        .optional()?;
    match (by_install, by_path) {
        (Some(bound), _) if bound != path => Err(MapError::new(
            MapErrorKind::Conflict,
            "this install id is bound to another archive path",
        )),
        (_, Some(bound)) if bound != install => Err(MapError::new(
            MapErrorKind::Conflict,
            "this archive path is bound to another install id",
        )),
        (None, None) => {
            let now = w.now()?;
            w.tx.execute(
                "INSERT INTO archive_installs(install, path, bound_at) VALUES (?1, ?2, ?3)",
                params![install, path, now],
            )?;
            Ok(())
        }
        _ => Ok(()),
    }
}

fn deadline_passed(deadline: Instant) -> Result<(), MapError> {
    if Instant::now() > deadline {
        return Err(MapError::new(
            MapErrorKind::Unavailable,
            "ingest ran past its time budget; nothing from this batch was kept",
        ));
    }
    Ok(())
}

impl MapStore {
    /// Reads up to `limit` (1..=100) coordinator events of `space` after this
    /// source's cursor and appends them as timeline events, atomically with
    /// the cursor. `source` is an explicit path to a `coord.sqlite`.
    ///
    /// # Errors
    ///
    /// `not_found`/`incompatible` for a missing, foreign, future, symlinked or
    /// non-private source; `conflict` for a replaced source or a rewritten row
    /// at the cursor; `invalid_input` for a row over the bounds (nothing kept);
    /// `unavailable` when the source stays locked or the time budget runs out.
    pub fn ingest_coordinator(
        &mut self,
        source: &Path,
        space: &str,
        limit: usize,
    ) -> Result<IngestReport, MapError> {
        self.ingest_coordinator_within(source, space, limit, IngestBudget::default())
    }

    /// [`MapStore::ingest_coordinator`] under an explicit [`IngestBudget`].
    ///
    /// # Errors
    ///
    /// As `ingest_coordinator`; `unavailable` when the budget interrupts.
    pub fn ingest_coordinator_within(
        &mut self,
        source: &Path,
        space: &str,
        limit: usize,
        budget: IngestBudget,
    ) -> Result<IngestReport, MapError> {
        validate_space(space)?;
        let sql = sql_limit(limit)?;
        let limit = page_limit(limit)?;
        let meter = Meter::new(budget);
        let mut src = open_source(source)?;
        meter.attach(&src.conn)?;
        meter.attach(self.reader())?;
        let out = self.coordinator_batch(&mut src, space, sql, limit, &meter);
        Meter::detach(self.reader());
        meter.result(out)
    }

    fn coordinator_batch(
        &mut self,
        src: &mut Source,
        space: &str,
        sql: i64,
        limit: usize,
        meter: &Meter,
    ) -> Result<IngestReport, MapError> {
        let deadline = meter.deadline;
        let snapshot = src.conn.transaction()?;
        check_coordinator(&snapshot)?;
        let kind = SourceKind::Coordinator;
        let meta = src.id.clone();
        self.write(|w| {
            let cur = cursor(w, kind, &meta, space)?;
            if cur.position > 0 {
                let held = snapshot
                    .query_row(&format!("{COORD_SELECT} AND id = ?2"), params![space, cur.position], read_coord)
                    .optional()?;
                if held.map(|r| r.hash(space)).as_deref() != Some(cur.last_hash.as_str()) {
                    return Err(MapError::new(
                        MapErrorKind::Conflict,
                        "the source no longer holds the event at the cursor as it was read; reset explicitly after checking it",
                    ));
                }
            }
            let mut stmt = snapshot.prepare(&format!("{COORD_SELECT} AND id > ?2 ORDER BY id LIMIT ?3"))?;
            let rows: Vec<CoordRow> =
                stmt.query_map(params![space, cur.position, sql], read_coord)?.collect::<Result<_, _>>()?;
            let mut report = IngestReport { caught_up: rows.len() <= limit, position: cur.position, ..IngestReport::default() };
            let mut last_hash = cur.last_hash;
            for row in rows.iter().take(limit) {
                deadline_passed(deadline)?;
                row.validate()?;
                let hash = row.hash(space);
                report.read += 1;
                if seen(w, kind, space, &hash)? {
                    report.already_seen += 1;
                } else {
                    let event = w.append(row.draft(space), Authority::Unverified)?;
                    mark(w, kind, space, &hash, &event.event_id())?;
                    report.appended += 1;
                }
                report.position = row.id;
                last_hash = hash;
            }
            if report.read > 0 {
                save_cursor(w, kind, &meta, space, report.position, &last_hash)?;
            }
            meter.passed()?;
            Meter::detach(&w.tx);
            Ok(report)
        })
    }

    /// Only when `include_archive_metadata` is set: reads up to `limit`
    /// archive entries after the cursor (client, session id, cwd, entry id;
    /// never text) and proposes `has_session` for every project whose live
    /// declared root contains the cwd by path components. Proposals carry
    /// basis `cwd_evidence` (600) and the number of matching roots; several
    /// candidates are all kept and labelled, none is chosen.
    ///
    /// # Errors
    ///
    /// As [`MapStore::ingest_coordinator`], for an archive of schema 3.
    pub fn ingest_archive_metadata(
        &mut self,
        source: &ArchiveSource<'_>,
        space: &str,
        include_archive_metadata: bool,
        limit: usize,
    ) -> Result<IngestReport, MapError> {
        self.ingest_archive_metadata_within(
            source,
            space,
            include_archive_metadata,
            limit,
            IngestBudget::default(),
        )
    }

    /// [`MapStore::ingest_archive_metadata`] under an explicit [`IngestBudget`].
    ///
    /// # Errors
    ///
    /// As `ingest_archive_metadata`; `unavailable` when the budget interrupts.
    pub fn ingest_archive_metadata_within(
        &mut self,
        source: &ArchiveSource<'_>,
        space: &str,
        include_archive_metadata: bool,
        limit: usize,
        budget: IngestBudget,
    ) -> Result<IngestReport, MapError> {
        if !include_archive_metadata {
            return Ok(IngestReport {
                caught_up: true,
                ..IngestReport::default()
            });
        }
        validate_install(source.install)?;
        validate_space(space)?;
        let sql = sql_limit(limit)?;
        let limit = page_limit(limit)?;
        let meter = Meter::new(budget);
        let mut src = open_source(source.path)?;
        // The cursor follows this install's file: same file under another id is another source.
        src.id.identity = format!("{}@{}", source.install, src.id.identity);
        meter.attach(&src.conn)?;
        meter.attach(self.reader())?;
        let out = self.archive_batch(&mut src, source.install, space, sql, limit, &meter);
        Meter::detach(self.reader());
        meter.result(out)
    }

    fn archive_batch(
        &mut self,
        src: &mut Source,
        install: &str,
        space: &str,
        sql: i64,
        limit: usize,
        meter: &Meter,
    ) -> Result<IngestReport, MapError> {
        let deadline = meter.deadline;
        let snapshot = src.conn.transaction()?;
        check_archive(&snapshot)?;
        let quarantine: bool = snapshot.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'quarantine')",
            [],
            |r| r.get(0),
        )?;
        let visible = if quarantine {
            " AND NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.entry_id = e.id AND q.released_at IS NULL)"
        } else {
            ""
        };
        let kind = SourceKind::Archive;
        let meta = src.id.clone();
        self.write(|w| {
            bind_install(w, install, &meta.path)?;
            let cur = cursor(w, kind, &meta, space)?;
            if cur.position > 0 {
                let held = snapshot
                    .query_row(&format!("{ARCHIVE_SELECT} AND e.id = ?1"), [cur.position], read_entry)
                    .optional()?;
                if held.map(|r| r.hash()).as_deref() != Some(cur.last_hash.as_str()) {
                    return Err(MapError::new(
                        MapErrorKind::Conflict,
                        "the archive no longer holds the entry at the cursor as it was read; reset explicitly after checking it",
                    ));
                }
            }
            let mut stmt =
                snapshot.prepare(&format!("{ARCHIVE_SELECT}{visible} AND e.id > ?1 ORDER BY e.id LIMIT ?2"))?;
            let rows: Vec<EntryRow> = stmt.query_map(params![cur.position, sql], read_entry)?.collect::<Result<_, _>>()?;
            let roots = declared_roots(w, space)?;
            let mut report = IngestReport { caught_up: rows.len() <= limit, position: cur.position, ..IngestReport::default() };
            let mut last_hash = cur.last_hash;
            // (root index, client, session) -> (matching entries, first entry, candidates)
            let mut groups: BTreeMap<(usize, String, String), (i64, i64, usize)> = BTreeMap::new();
            for row in rows.iter().take(limit) {
                deadline_passed(deadline)?;
                report.read += 1;
                report.position = row.id;
                last_hash = row.hash();
                let Some((client, session, cwd)) = row.usable() else {
                    report.unmatched += 1;
                    continue;
                };
                let matched: Vec<usize> = (0..roots.len()).filter(|&i| cwd.starts_with(&roots[i].path)).collect();
                if matched.is_empty() {
                    report.unmatched += 1;
                }
                for &i in &matched {
                    let g = groups.entry((i, client.to_owned(), session.to_owned())).or_insert((0, row.id, 0));
                    g.0 += 1;
                    g.2 = g.2.max(matched.len());
                }
            }
            for ((i, client, session), (count, first, candidates)) in groups {
                let root = &roots[i];
                let object = format!("archive-session:{client}/{session}");
                let entry_ref = format!("archive:{install}/{first}");
                if validate_ref("object", &object).is_err() || validate_ref("evidence", &entry_ref).is_err() {
                    report.unmatched += 1;
                    continue;
                }
                let key = fields_hash(&["archive-link/2", install, &root.project, &object]);
                if seen(w, kind, space, &key)? {
                    report.already_seen += 1;
                    continue;
                }
                let mut evidence = vec![entry_ref];
                evidence.extend(root.evidence.iter().take(MAX_EVIDENCE - 1).cloned());
                let draft = EventDraft {
                    kind: "link_proposed".into(),
                    space: space.into(),
                    subject: root.project.clone(),
                    relation: "has_session".into(),
                    object,
                    basis: "cwd_evidence".into(),
                    asserted_kind: "collector".into(),
                    client: "archive".into(),
                    agent: "metadata".into(),
                    evidence,
                    occurred_at: None,
                    occurred_at_source: "unknown".into(),
                    confidence_milli: 600,
                    corrects: None,
                    payload: json!({"count": count, "candidates": candidates}).to_string(),
                };
                let event = w.append(draft, Authority::Unverified)?;
                mark(w, kind, space, &key, &event.event_id())?;
                report.appended += 1;
            }
            if report.read > 0 {
                save_cursor(w, kind, &meta, space, report.position, &last_hash)?;
            }
            meter.passed()?;
            Meter::detach(&w.tx);
            Ok(report)
        })
    }

    /// Forgets the cursors of one source path in one space, so a replaced or
    /// rewritten source can be read again. Rows already ingested with the same
    /// content are recognised and not appended twice. Needs the trusted host's grant.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a bad space or path; storage errors.
    pub fn reset_ingest_cursor(
        &mut self,
        kind: SourceKind,
        source: &Path,
        space: &str,
        _grant: LocalUserGrant,
    ) -> Result<usize, MapError> {
        validate_space(space)?;
        let path = source
            .to_str()
            .ok_or_else(|| MapError::invalid("source path is not UTF-8"))?;
        self.write(|w| {
            if kind == SourceKind::Archive {
                w.tx.execute("DELETE FROM archive_installs WHERE path = ?1", [path])?;
            }
            Ok(w.tx.execute(
                "DELETE FROM ingest_cursors WHERE source_kind = ?1 AND path = ?2 AND space = ?3",
                params![kind.as_str(), path, space],
            )?)
        })
    }

    /// Maps `archive:<install>/<entry>` to a trusted, host-configured archive
    /// and entry id. The path always comes from `trusted`, never from the ref,
    /// and must be the one this map bound the install to when it ingested.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a malformed ref, `not_found` for an install the host
    /// did not configure or this map never read, `conflict` when the host's
    /// path differs from the bound one.
    pub fn resolve_archive_ref(
        &self,
        reference: &str,
        trusted: &[ArchiveSource<'_>],
    ) -> Result<(PathBuf, i64), MapError> {
        let bad = || MapError::invalid("not an archive:<install>/<entry> ref");
        let (install, entry) = reference
            .strip_prefix("archive:")
            .and_then(|r| r.split_once('/'))
            .ok_or_else(bad)?;
        validate_install(install)?;
        if entry.is_empty() || entry.len() > 18 || !entry.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let entry: i64 = entry.parse().map_err(|_| bad())?;
        if entry < 1 {
            return Err(bad());
        }
        let source = trusted
            .iter()
            .find(|s| s.install == install)
            .ok_or_else(|| {
                MapError::new(
                    MapErrorKind::NotFound,
                    "this install is not configured on this host",
                )
            })?;
        let bound: Option<String> = self
            .reader()
            .query_row(
                "SELECT path FROM archive_installs WHERE install = ?1",
                [install],
                |r| r.get(0),
            )
            .optional()?;
        match bound {
            None => Err(MapError::new(
                MapErrorKind::NotFound,
                "this map never read that install",
            )),
            Some(p) if Path::new(&p) != source.path => Err(MapError::new(
                MapErrorKind::Conflict,
                "the host's path for this install differs from the bound one",
            )),
            Some(_) => Ok((source.path.to_path_buf(), entry)),
        }
    }

    /// Timeline of one space (optionally one task) in local receipt order after
    /// `after` (a `local_seq`), at most `limit` ≤ 100. With `as_of_recorded`,
    /// only what this map had received by then. Read from the ledger itself,
    /// so a rebuild cannot change it.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a bad space or limit.
    pub fn timeline(
        &self,
        space: &str,
        task: Option<i64>,
        after: i64,
        limit: usize,
        as_of_recorded: Option<i64>,
    ) -> Result<(Vec<TimelineEntry>, Option<i64>), MapError> {
        validate_space(space)?;
        let sql = sql_limit(limit)?;
        let subject = task.map(|t| format!("task:{space}/{t}"));
        let mut stmt = self.reader().prepare(&format!(
            "SELECT {COLUMNS}, local_seq, received_at FROM events WHERE space = ?1 \
             AND kind IN ('coord_recorded', 'outcome_claimed', 'outcome_observed') AND (?2 IS NULL OR subject = ?2) \
             AND local_seq > ?3 AND (?4 IS NULL OR received_at <= ?4) ORDER BY local_seq LIMIT ?5"
        ))?;
        let rows = stmt.query_map(params![space, subject, after, as_of_recorded, sql], |r| {
            Ok((read_event(r)?, r.get::<_, i64>(21)?, r.get::<_, i64>(22)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (e, local_seq, received_at) = row?;
            let payload: Value = serde_json::from_str(&e.draft.payload).unwrap_or(Value::Null);
            let text = |k: &str| {
                payload
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            let int = |k: &str| payload.get(k).and_then(Value::as_i64);
            out.push(TimelineEntry {
                event_id: e.event_id(),
                local_seq,
                subject: e.draft.subject.clone(),
                kind: e.draft.kind.clone(),
                coord_kind: text("coord_kind"),
                outcome: e
                    .draft
                    .object
                    .split_once('/')
                    .map_or("", |(_, o)| o)
                    .to_owned(),
                basis: e.draft.basis.clone(),
                agent: e.draft.agent.clone(),
                actor_machine: text("actor_machine"),
                fence: int("fence"),
                claim_id: int("claim_id"),
                source_at: int("source_at"),
                recorded_at: e.recorded_at,
                received_at,
                evidence: e.draft.evidence.clone(),
            });
        }
        let next = (out.len() > limit).then(|| out[limit - 1].local_seq);
        out.truncate(limit);
        Ok((out, next))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use rusqlite::{Connection, OpenFlags, params};
    use serde_json::Value;

    use super::*;
    use crate::coordination::{Actor, Coordinator};

    const M: &str = "m-dddddddddddddddddddddddddddddddd";
    const N: &str = "m-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const NOTE_CANARY: &str = "NOTE-CANARY-7f3a";
    const TEXT_CANARY: &str = "ARCHIVE-TEXT-CANARY-91be";

    struct Fx {
        dir: PathBuf,
        map_clock: Arc<AtomicI64>,
        coord_clock: Arc<AtomicI64>,
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn private_dir(path: &Path) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
    }

    fn fx(_tag: &str) -> Fx {
        let dir = crate::test_support::temp_path("brain-m2");
        let _ = fs::remove_dir_all(&dir);
        private_dir(&dir);
        Fx {
            dir,
            map_clock: Arc::new(AtomicI64::new(5_000)),
            coord_clock: Arc::new(AtomicI64::new(100)),
        }
    }

    impl Fx {
        fn coord_path(&self) -> PathBuf {
            self.dir.join("coord").join("coord.sqlite")
        }
        fn coordinator(&self) -> Coordinator {
            let clock = Arc::clone(&self.coord_clock);
            Coordinator::open(&self.coord_path())
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst))
        }
        fn map_path(&self, name: &str) -> PathBuf {
            self.dir.join(name).join("map.sqlite")
        }
        fn map(&self, name: &str, machine: &str) -> MapStore {
            let clock = Arc::clone(&self.map_clock);
            MapStore::open(&self.map_path(name), machine)
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst))
        }
        /// A private copy of `from` in its own 0700 directory.
        fn copy(&self, from: &Path, name: &str) -> PathBuf {
            let dir = self.dir.join(name);
            private_dir(&dir);
            let to = dir.join("copy.sqlite");
            fs::copy(from, &to).unwrap();
            fs::set_permissions(&to, fs::Permissions::from_mode(0o600)).unwrap();
            to
        }
    }

    fn task_id(v: &Value) -> i64 {
        v["task"]["id"].as_i64().unwrap()
    }

    fn claim(v: &Value) -> (i64, i64) {
        (
            v["claim"]["claim_id"].as_i64().unwrap(),
            v["claim"]["fence"].as_i64().unwrap(),
        )
    }

    /// In `sp`: a completed task, a failed one, and one whose lease expired and
    /// was taken over. In `other`: one task. Notes carry a canary.
    fn history(f: &Fx) -> (i64, i64, i64) {
        let mut c = f.coordinator();
        let a = Actor::new("sp", "codex", "m-aaa").unwrap();
        let b = Actor::new("sp", "claude", "m-bbb").unwrap();
        let o = Actor::new("other", "zed", "m-ccc").unwrap();
        let t1 = task_id(
            &c.add_task(&a, "p", NOTE_CANARY, NOTE_CANARY, &[], None)
                .unwrap(),
        );
        let (c1, f1) = claim(&c.claim_task(&a, t1, 30, None).unwrap());
        c.set_state(&a, c1, f1, "completed", NOTE_CANARY, None)
            .unwrap();
        let t2 = task_id(&c.add_task(&a, "p", "second", "", &[], None).unwrap());
        let (c2, f2) = claim(&c.claim_task(&b, t2, 30, None).unwrap());
        c.set_state(&b, c2, f2, "failed", NOTE_CANARY, None)
            .unwrap();
        let t3 = task_id(&c.add_task(&a, "p", "third", "", &[t1], None).unwrap());
        c.claim_task(&a, t3, 10, None).unwrap();
        f.coord_clock.store(200, Ordering::SeqCst);
        c.claim_task(&b, t3, 30, None).unwrap();
        c.add_task(&o, "q", "elsewhere", "", &[], None).unwrap();
        (t1, t2, t3)
    }

    fn source_ids(path: &Path, space: &str) -> Vec<i64> {
        let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = c
            .prepare("SELECT id FROM events WHERE space = ?1 ORDER BY id")
            .unwrap();
        stmt.query_map([space], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The database and its WAL: the data. `-shm` is SQLite's shared lock index, not content.
    fn file_bytes(path: &Path) -> Vec<u8> {
        let mut out = fs::read(path).unwrap();
        out.extend(fs::read(format!("{}-wal", path.display())).unwrap_or_default());
        out
    }

    fn contains(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    fn ledger_len(m: &MapStore) -> usize {
        let mut n = 0;
        let mut cursor = 0;
        loop {
            let (page, next) = m.trusted_export_page(cursor, 100).unwrap();
            n += page.len();
            match next {
                Some(c) => cursor = c,
                None => return n,
            }
        }
    }

    fn count(m: &MapStore, table: &str) -> i64 {
        m.reader()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn timeline(
        m: &MapStore,
        space: &str,
        task: Option<i64>,
        as_of: Option<i64>,
    ) -> Vec<TimelineEntry> {
        let (mut out, mut after) = (Vec::new(), 0);
        loop {
            let (page, next) = m.timeline(space, task, after, 3, as_of).unwrap();
            out.extend(page);
            match next {
                Some(n) => after = n,
                None => return out,
            }
        }
    }

    fn ingest_arch(
        m: &mut MapStore,
        path: &Path,
        space: &str,
        include: bool,
        limit: usize,
    ) -> Result<IngestReport, MapError> {
        m.ingest_archive_metadata(
            &ArchiveSource {
                install: "inst-a",
                path,
            },
            space,
            include,
            limit,
        )
    }

    fn kind<T: std::fmt::Debug>(r: Result<T, MapError>) -> MapErrorKind {
        r.expect_err("expected error").kind
    }

    #[test]
    fn coordinator_history_becomes_claimed_and_unknown_outcomes_never_observed() {
        let f = fx("history");
        let (t1, t2, t3) = history(&f);
        let src = f.coord_path();
        let before = file_bytes(&src);
        let mut m = f.map("m", M);
        let r = m.ingest_coordinator(&src, "sp", 100).unwrap();
        let ids = source_ids(&src, "sp");
        assert_eq!(
            (r.read, r.appended, r.caught_up, r.position),
            (ids.len(), ids.len(), true, *ids.last().unwrap())
        );
        let words = |t| {
            timeline(&m, "sp", Some(t), None)
                .into_iter()
                .map(|e| (e.coord_kind, e.outcome))
                .collect::<Vec<_>>()
        };
        let pair = |k: &str, o: &str| (k.to_owned(), o.to_owned());
        assert_eq!(
            words(t1),
            vec![
                pair("created", ""),
                pair("claimed", ""),
                pair("completed", "claimed_completed")
            ]
        );
        assert_eq!(
            words(t2),
            vec![
                pair("created", ""),
                pair("claimed", ""),
                pair("failed", "claimed_failed")
            ]
        );
        assert_eq!(
            words(t3),
            vec![
                pair("created", ""),
                pair("claimed", ""),
                pair("lease_expired", "unknown_lease_expired"),
                pair("claimed", "")
            ]
        );
        let done = timeline(&m, "sp", Some(t1), None).pop().unwrap();
        assert_eq!(
            (done.kind.as_str(), done.basis.as_str(), done.agent.as_str()),
            ("outcome_claimed", "coordinator_record", "codex")
        );
        assert_eq!(done.actor_machine, "m-aaa");
        assert!(done.fence.is_some() && done.claim_id.is_some());
        // Source clock and receipt clock stay apart.
        assert_eq!(
            (done.source_at, done.recorded_at, done.received_at),
            (Some(100), 5_000, 5_000)
        );
        assert!(done.evidence[0].starts_with("coord-event:sp/"));
        let expired = timeline(&m, "sp", Some(t3), None)
            .into_iter()
            .find(|e| e.coord_kind == "lease_expired")
            .unwrap();
        assert_eq!(
            (expired.agent.as_str(), expired.actor_machine.as_str()),
            ("codex", "m-aaa")
        );
        let all = timeline(&m, "sp", None, None);
        assert_eq!(all.len(), ids.len());
        assert!(
            all.iter()
                .all(|e| e.kind != "outcome_observed" && e.basis == "coordinator_record")
        );
        // No current status is inferred and nothing from `other` arrived.
        assert!(m.links_page("sp", None, 100).unwrap().0.is_empty());
        assert!(timeline(&m, "other", None, None).is_empty());
        // Notes, titles and details never reach the map; the source is untouched.
        let map_bytes = file_bytes(&f.map_path("m"));
        assert!(!contains(&map_bytes, NOTE_CANARY));
        assert!(before == file_bytes(&src), "the source changed");
    }

    #[test]
    fn batches_are_idempotent_and_each_space_keeps_its_cursor() {
        let f = fx("batches");
        history(&f);
        let src = f.coord_path();
        let mut m = f.map("m", M);
        let (mut sp, mut other) = (0, 0);
        loop {
            let a = m.ingest_coordinator(&src, "sp", 2).unwrap();
            let b = m.ingest_coordinator(&src, "other", 1).unwrap();
            sp += a.appended;
            other += b.appended;
            if a.caught_up && b.caught_up {
                break;
            }
        }
        assert_eq!(
            (sp, other),
            (
                source_ids(&src, "sp").len(),
                source_ids(&src, "other").len()
            )
        );
        for space in ["sp", "other"] {
            let again = m.ingest_coordinator(&src, space, 100).unwrap();
            assert_eq!((again.read, again.appended, again.caught_up), (0, 0, true));
        }
        let evidence: Vec<String> = timeline(&m, "sp", None, None)
            .into_iter()
            .map(|e| e.evidence[0].clone())
            .collect();
        let expected: Vec<String> = source_ids(&src, "sp")
            .iter()
            .map(|id| format!("coord-event:sp/{id}"))
            .collect();
        assert_eq!(
            evidence, expected,
            "every source event once, in source order"
        );
        // New source events: only they are appended.
        let mut c = f.coordinator();
        c.add_task(
            &Actor::new("sp", "codex", "m-aaa").unwrap(),
            "p",
            "late",
            "",
            &[],
            None,
        )
        .unwrap();
        drop(c);
        assert_eq!(m.ingest_coordinator(&src, "sp", 100).unwrap().appended, 1);
        assert_eq!(
            ledger_len(&m),
            source_ids(&src, "sp").len() + source_ids(&src, "other").len()
        );
    }

    #[test]
    fn a_bad_row_keeps_nothing_from_its_batch_and_the_cursor_waits() {
        let f = fx("badrow");
        history(&f);
        let src = f.coord_path();
        let good = source_ids(&src, "sp").len();
        let raw = Connection::open(&src).unwrap();
        raw.execute("INSERT INTO events(space, kind, agent, machine, at) VALUES ('sp', 'created', 'bad\u{7}agent', 'm-aaa', 300)", [])
            .unwrap();
        raw.execute("INSERT INTO events(space, kind, agent, machine, at) VALUES ('sp', 'created', 'codex', 'm-aaa', 301)", [])
            .unwrap();
        drop(raw);
        let mut m = f.map("m", M);
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 100)),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            (
                ledger_len(&m),
                count(&m, "ingest_cursors"),
                count(&m, "ingest_seen")
            ),
            (0, 0, 0)
        );
        let r = m.ingest_coordinator(&src, "sp", good).unwrap();
        assert_eq!((r.appended, r.caught_up), (good, false));
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 100)),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            ledger_len(&m),
            good,
            "the row after the bad one is not skipped past it"
        );
        let long = "a".repeat(65);
        let raw = Connection::open(&src).unwrap();
        raw.execute("INSERT INTO events(space, kind, agent, machine, at) VALUES ('x', 'created', ?1, 'm-aaa', 1)", [&long])
            .unwrap();
        drop(raw);
        assert_eq!(
            kind(m.ingest_coordinator(&src, "x", 100)),
            MapErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_replaced_or_rewritten_source_needs_an_explicit_reset() {
        let f = fx("replace");
        history(&f);
        let src = f.coord_path();
        let mut m = f.map("m", M);
        m.ingest_coordinator(&src, "sp", 100).unwrap();
        let held = ledger_len(&m);
        // Same path, another file (new inode, same content).
        let tmp = src.with_extension("new");
        fs::copy(&src, &tmp).unwrap();
        fs::rename(&tmp, &src).unwrap();
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 100)),
            MapErrorKind::Conflict
        );
        assert_eq!(ledger_len(&m), held);
        let grant = LocalUserGrant::from_direct_local_action();
        assert_eq!(
            m.reset_ingest_cursor(SourceKind::Coordinator, &src, "sp", grant)
                .unwrap(),
            1
        );
        let r = m.ingest_coordinator(&src, "sp", 100).unwrap();
        assert_eq!(
            (r.appended, r.already_seen),
            (0, source_ids(&src, "sp").len()),
            "identical rows are not doubled"
        );
        let mut c = f.coordinator();
        c.add_task(
            &Actor::new("sp", "codex", "m-aaa").unwrap(),
            "p",
            "new",
            "",
            &[],
            None,
        )
        .unwrap();
        drop(c);
        assert_eq!(m.ingest_coordinator(&src, "sp", 100).unwrap().appended, 1);
        // The row under the cursor rewritten in place: refused, not re-read.
        let raw = Connection::open(&src).unwrap();
        raw.execute("UPDATE events SET agent = 'mallory' WHERE id = (SELECT max(id) FROM events WHERE space = 'sp')", [])
            .unwrap();
        drop(raw);
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 100)),
            MapErrorKind::Conflict
        );
        assert_eq!(ledger_len(&m), held + 1);
        // Another space of the same file has its own cursor.
        assert!(m.ingest_coordinator(&src, "other", 100).unwrap().appended > 0);
    }

    #[test]
    fn sources_are_checked_before_anything_is_read() {
        let f = fx("checks");
        history(&f);
        let src = f.coord_path();
        let mut m = f.map("m", M);
        let ingest = |m: &mut MapStore, p: &Path| kind(m.ingest_coordinator(p, "sp", 10));
        assert_eq!(
            ingest(&mut m, Path::new("coord/coord.sqlite")),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            ingest(&mut m, &f.dir.join("coord/../coord/coord.sqlite")),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            ingest(&mut m, &f.dir.join("coord/missing.sqlite")),
            MapErrorKind::NotFound
        );
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 0)),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            kind(m.ingest_coordinator(&src, "sp", 101)),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            kind(m.ingest_coordinator(&src, "s p", 10)),
            MapErrorKind::InvalidInput
        );
        let link_dir = f.dir.join("link");
        private_dir(&link_dir);
        std::os::unix::fs::symlink(&src, link_dir.join("coord.sqlite")).unwrap();
        assert_eq!(
            ingest(&mut m, &link_dir.join("coord.sqlite")),
            MapErrorKind::Incompatible
        );
        std::os::unix::fs::symlink(src.parent().unwrap(), f.dir.join("linkdir")).unwrap();
        assert_eq!(
            ingest(&mut m, &f.dir.join("linkdir").join("coord.sqlite")),
            MapErrorKind::Incompatible
        );
        let open = f.copy(&src, "open");
        fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ingest(&mut m, &open), MapErrorKind::Incompatible);
        let shared = f.copy(&src, "shared");
        fs::set_permissions(shared.parent().unwrap(), fs::Permissions::from_mode(0o750)).unwrap();
        assert_eq!(ingest(&mut m, &shared), MapErrorKind::Incompatible);
        let side = f.copy(&src, "side");
        std::os::unix::fs::symlink(&src, format!("{}-wal", side.display())).unwrap();
        assert_eq!(ingest(&mut m, &side), MapErrorKind::Incompatible);
        let future = f.copy(&src, "future");
        Connection::open(&future)
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert_eq!(ingest(&mut m, &future), MapErrorKind::Incompatible);
        let shape = f.copy(&src, "shape");
        Connection::open(&shape)
            .unwrap()
            .execute_batch("ALTER TABLE events ADD COLUMN extra TEXT")
            .unwrap();
        assert_eq!(ingest(&mut m, &shape), MapErrorKind::Incompatible);
        let foreign_dir = f.dir.join("foreign");
        private_dir(&foreign_dir);
        let foreign = foreign_dir.join("x.sqlite");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch("CREATE TABLE events(id INTEGER)")
            .unwrap();
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(ingest(&mut m, &foreign), MapErrorKind::Incompatible);
        let corrupt = f.copy(&src, "corrupt");
        fs::write(
            &corrupt,
            b"definitely not sqlite, just bytes long enough to look like a header....",
        )
        .unwrap();
        assert_ne!(
            kind(m.ingest_coordinator(&corrupt, "sp", 10)),
            MapErrorKind::InvalidInput
        );
        assert_eq!(
            kind(ingest_arch(&mut m, &src, "sp", true, 10)),
            MapErrorKind::Incompatible
        );
        assert_eq!(
            (ledger_len(&m), count(&m, "ingest_cursors")),
            (0, 0),
            "no failed check left a trace"
        );
    }

    #[test]
    fn timeline_as_of_rebuild_and_replay_agree() {
        let f = fx("asof");
        history(&f);
        let src = f.coord_path();
        let mut m = f.map("m", M);
        f.map_clock.store(1_000, Ordering::SeqCst);
        m.ingest_coordinator(&src, "sp", 4).unwrap();
        f.map_clock.store(2_000, Ordering::SeqCst);
        m.ingest_coordinator(&src, "sp", 100).unwrap();
        assert!(timeline(&m, "sp", None, Some(999)).is_empty());
        assert_eq!(timeline(&m, "sp", None, Some(1_500)).len(), 4);
        let full = timeline(&m, "sp", None, None);
        assert_eq!(full.len(), source_ids(&src, "sp").len());
        let links = m.links_dump().unwrap();
        m.rebuild().unwrap();
        assert_eq!(
            (timeline(&m, "sp", None, None), m.links_dump().unwrap()),
            (full.clone(), links)
        );
        // Another machine replaying the ledger reads the same history.
        let mut other = f.map("n", N);
        let (events, _) = m.trusted_export_page(0, 100).unwrap();
        for e in &events {
            other.replay(e).unwrap();
        }
        let key = |t: &[TimelineEntry]| {
            t.iter()
                .map(|e| {
                    (
                        e.subject.clone(),
                        e.coord_kind.clone(),
                        e.outcome.clone(),
                        e.source_at,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(key(&timeline(&other, "sp", None, None)), key(&full));
        assert_eq!(
            kind(m.timeline("sp", None, 0, 0, None)),
            MapErrorKind::InvalidInput
        );
    }

    const ARCHIVE_DDL: &str = "
        CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT NOT NULL, path TEXT NOT NULL,
            path_sha256 TEXT NOT NULL, UNIQUE(client, path));
        CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES sources(id),
            session_id TEXT, role TEXT NOT NULL, locator TEXT NOT NULL, source_offset INTEGER, ts TEXT,
            content TEXT NOT NULL, content_sha256 TEXT NOT NULL, evidence_sha256 TEXT NOT NULL,
            record_sha256 TEXT NOT NULL, imported_at TEXT NOT NULL, UNIQUE(source_id, locator, evidence_sha256));
        CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id',
            tokenize=\"unicode61 remove_diacritics 0 tokenchars '_'\");
        CREATE TABLE cursors(source_id INTEGER PRIMARY KEY REFERENCES sources(id), client TEXT NOT NULL,
            dev INTEGER, ino INTEGER, size INTEGER, position INTEGER NOT NULL DEFAULT 0,
            discarding INTEGER NOT NULL DEFAULT 0, head_len INTEGER NOT NULL DEFAULT 0, head_sha256 TEXT,
            ctx TEXT, state TEXT NOT NULL, last_error TEXT, resets INTEGER NOT NULL DEFAULT 0,
            last_reset_reason TEXT, malformed INTEGER NOT NULL DEFAULT 0, oversize INTEGER NOT NULL DEFAULT 0,
            first_seen TEXT NOT NULL, last_visit TEXT, last_caught_up TEXT);
        INSERT INTO sources VALUES (1, 'codex', '/synthetic/codex.jsonl', 'x'), (2, 'claude', '/synthetic/claude.jsonl', 'y');
    ";

    type Entry<'a> = (i64, Option<&'a str>, Option<&'a str>);

    fn archive(f: &Fx, name: &str, marker: &str, with_cwd: bool, entries: &[Entry<'_>]) -> PathBuf {
        let dir = f.dir.join(name);
        private_dir(&dir);
        let path = dir.join("archive.sqlite");
        let c = Connection::open(&path).unwrap();
        c.execute_batch(ARCHIVE_DDL).unwrap();
        c.execute("INSERT INTO archive_meta VALUES ('schema', ?1)", [marker])
            .unwrap();
        if with_cwd {
            c.execute_batch("ALTER TABLE entries ADD COLUMN cwd TEXT")
                .unwrap();
        }
        drop(c);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        add_entries(&path, entries);
        path
    }

    fn add_entries(path: &Path, entries: &[Entry<'_>]) {
        let c = Connection::open(path).unwrap();
        for (source, session, cwd) in entries {
            let n: i64 = c
                .query_row("SELECT coalesce(max(id), 0) + 1 FROM entries", [], |r| {
                    r.get(0)
                })
                .unwrap();
            c.execute(
                "INSERT INTO entries(id, source_id, session_id, role, locator, ts, content, content_sha256, \
                 evidence_sha256, record_sha256, imported_at, cwd) VALUES (?1, ?2, ?3, 'user', ?4, \
                 '2026-10-01T00:00:00Z', ?5, 'c', 'e', ?6, '2026-10-01T00:00:00Z', ?7)",
                params![n, source, session, format!("l{n}"), format!("{TEXT_CANARY} {n}"), format!("r{n}"), cwd],
            )
            .unwrap();
        }
    }

    fn declare(m: &mut MapStore, project: &str, root: &str, evidence: bool) -> crate::map::Event {
        let d = EventDraft {
            kind: "link_proposed".into(),
            space: "sp".into(),
            subject: format!("project:sp/{project}"),
            relation: "declared_root".into(),
            object: format!("project:sp/{root}"),
            basis: "agent_claim".into(),
            asserted_kind: "agent".into(),
            client: "codex".into(),
            agent: "codex".into(),
            evidence: if evidence {
                vec!["memory:sp/1".into()]
            } else {
                Vec::new()
            },
            occurred_at_source: "unknown".into(),
            confidence_milli: 700,
            ..EventDraft::default()
        };
        m.append(d, Authority::Unverified).unwrap()
    }

    fn sessions(m: &MapStore) -> Vec<(String, String)> {
        m.links_page("sp", None, 100)
            .unwrap()
            .0
            .into_iter()
            .filter(|r| r.relation == "has_session")
            .inspect(|r| {
                assert_eq!(
                    (r.status.as_str(), r.best_basis.as_str(), r.confidence_milli),
                    ("proposed", "cwd_evidence", 600)
                );
                assert!(!r.contested);
            })
            .map(|r| {
                (
                    r.subject.trim_start_matches("project:sp/").to_owned(),
                    r.object.trim_start_matches("archive-session:").to_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn archive_metadata_is_opt_in_matches_path_components_and_keeps_text_out() {
        let f = fx("archive");
        let mut m = f.map("m", M);
        declare(&mut m, "proj", "/a/proj", true);
        declare(&mut m, "project", "/a/project", true);
        declare(&mut m, "outer", "/a", true);
        declare(&mut m, "ghost", "/a/proj", false);
        let gone = declare(&mut m, "gone", "/a/proj", true);
        let retract = EventDraft {
            kind: "correction".into(),
            object: String::new(),
            corrects: Some(gone.event_id()),
            ..gone.draft.clone()
        };
        m.append(retract, Authority::Unverified).unwrap();
        let held = ledger_len(&m);
        // Off unless asked: the path is not even opened.
        let off = ingest_arch(&mut m, &f.dir.join("nowhere.sqlite"), "sp", false, 10).unwrap();
        assert_eq!((off.read, off.appended, ledger_len(&m)), (0, 0, held));
        let src = archive(
            &f,
            "arch",
            ARCHIVE_SCHEMA,
            true,
            &[
                (1, Some("s1"), Some("/a/proj/src")),
                (1, Some("s1"), Some("/a/proj")),
                (2, Some("s2"), Some("/a/project/x")),
                (1, Some("s3"), Some("/a/projector")),
                (1, None, Some("/a/proj")),
                (1, Some("s4"), None),
                (1, Some("s5"), Some("a/proj")),
                (1, Some("s6"), Some("/a/proj/../etc")),
            ],
        );
        let before = file_bytes(&src);
        let r = ingest_arch(&mut m, &src, "sp", true, 100).unwrap();
        assert_eq!(
            (r.read, r.unmatched, r.appended, r.caught_up),
            (8, 4, 5, true)
        );
        let mut got = sessions(&m);
        got.sort();
        let want: Vec<(String, String)> = [
            ("outer", "claude/s2"),
            ("outer", "codex/s1"),
            ("outer", "codex/s3"),
            ("proj", "codex/s1"),
            ("project", "claude/s2"),
        ]
        .iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect();
        assert_eq!(
            got, want,
            "/a/projector is not under /a/proj; ghost and retracted roots are unused"
        );
        let (events, _) = m.events_page("sp", 0, 100).unwrap();
        let proj = events
            .iter()
            .find(|e| e.draft.subject == "project:sp/proj" && e.draft.relation == "has_session")
            .unwrap();
        assert_eq!(
            proj.draft.payload, "{\"candidates\":2,\"count\":2}",
            "two roots matched: labelled, not chosen"
        );
        assert_eq!(
            proj.draft.evidence,
            vec!["archive:inst-a/1".to_owned(), "memory:sp/1".to_owned()]
        );
        assert_eq!(
            (proj.authority.as_str(), proj.draft.asserted_kind.as_str()),
            ("unverified", "collector")
        );
        let map_bytes = file_bytes(&f.map_path("m"));
        for secret in [TEXT_CANARY, "/a/proj/src", "/a/projector", "/a/project/x"] {
            assert!(
                !contains(&map_bytes, secret),
                "{secret} reached the map file"
            );
        }
        assert!(before == file_bytes(&src), "the source changed");
        // Idempotent, then incremental; quarantined entries are not read.
        let again = ingest_arch(&mut m, &src, "sp", true, 100).unwrap();
        assert_eq!((again.read, again.appended), (0, 0));
        add_entries(
            &src,
            &[
                (1, Some("s1"), Some("/a/proj/lib")),
                (1, Some("s7"), Some("/a/proj")),
            ],
        );
        let c = Connection::open(&src).unwrap();
        c.execute_batch(
            "CREATE TABLE quarantine(entry_id INTEGER PRIMARY KEY, reason TEXT NOT NULL, rule TEXT NOT NULL, \
             evidence TEXT NOT NULL, quarantined_at TEXT NOT NULL, released_at TEXT);
             INSERT INTO quarantine VALUES (10, 'r', 'rule', 'e', 't', NULL);",
        )
        .unwrap();
        drop(c);
        let late = ingest_arch(&mut m, &src, "sp", true, 100).unwrap();
        assert_eq!((late.read, late.appended, late.already_seen), (1, 0, 2));
        assert!(!sessions(&m).iter().any(|(_, s)| s == "codex/s7"));
    }

    #[test]
    fn archive_layouts_and_oversized_metadata_are_refused_or_skipped() {
        let f = fx("archshape");
        let mut m = f.map("m", M);
        declare(&mut m, "proj", "/a/proj", true);
        let held = ledger_len(&m);
        let old = archive(
            &f,
            "v2",
            "brain_session_archive:2",
            true,
            &[(1, Some("s1"), Some("/a/proj"))],
        );
        assert_eq!(
            kind(ingest_arch(&mut m, &old, "sp", true, 10)),
            MapErrorKind::Incompatible
        );
        let no_cwd = archive(&f, "nocwd", ARCHIVE_SCHEMA, false, &[]);
        assert_eq!(
            kind(ingest_arch(&mut m, &no_cwd, "sp", true, 10)),
            MapErrorKind::Incompatible
        );
        let open = archive(&f, "open", ARCHIVE_SCHEMA, true, &[]);
        fs::set_permissions(&open, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            kind(ingest_arch(&mut m, &open, "sp", true, 10)),
            MapErrorKind::Incompatible
        );
        let long = "x".repeat(200);
        let odd = archive(
            &f,
            "odd",
            ARCHIVE_SCHEMA,
            true,
            &[
                (1, Some(&long), Some("/a/proj")),
                (1, Some("s\u{1}"), Some("/a/proj")),
            ],
        );
        let r = ingest_arch(&mut m, &odd, "sp", true, 10).unwrap();
        assert_eq!((r.read, r.unmatched, r.appended), (2, 2, 0));
        assert_eq!(ledger_len(&m), held);
    }

    #[test]
    fn two_installs_with_the_same_client_and_entry_ids_get_distinct_resolvable_refs() {
        let f = fx("installs");
        let mut m = f.map("m", M);
        declare(&mut m, "proj", "/a/proj", true);
        let same = [(1, Some("s1"), Some("/a/proj/src"))];
        let pa = archive(&f, "host-a", ARCHIVE_SCHEMA, true, &same);
        let pb = archive(&f, "host-b", ARCHIVE_SCHEMA, true, &same);
        let (a, b) = (
            ArchiveSource {
                install: "host-a",
                path: &pa,
            },
            ArchiveSource {
                install: "host-b",
                path: &pb,
            },
        );
        assert_eq!(
            m.ingest_archive_metadata(&a, "sp", true, 10)
                .unwrap()
                .appended,
            1
        );
        assert_eq!(
            m.ingest_archive_metadata(&b, "sp", true, 10)
                .unwrap()
                .appended,
            1
        );
        let (events, _) = m.events_page("sp", 0, 100).unwrap();
        let refs: Vec<String> = events
            .iter()
            .filter(|e| e.draft.relation == "has_session")
            .map(|e| e.draft.evidence[0].clone())
            .collect();
        assert_eq!(
            refs,
            vec!["archive:host-a/1".to_owned(), "archive:host-b/1".to_owned()]
        );
        let trusted = [a, b];
        assert_eq!(
            m.resolve_archive_ref(&refs[0], &trusted).unwrap(),
            (pa.clone(), 1)
        );
        assert_eq!(
            m.resolve_archive_ref(&refs[1], &trusted).unwrap(),
            (pb.clone(), 1)
        );
        // The path always comes from the host; a ref cannot name one.
        assert_eq!(
            kind(m.resolve_archive_ref("archive:host-c/1", &trusted)),
            MapErrorKind::NotFound
        );
        let swapped = [ArchiveSource {
            install: "host-a",
            path: &pb,
        }];
        assert_eq!(
            kind(m.resolve_archive_ref(&refs[0], &swapped)),
            MapErrorKind::Conflict
        );
        for bad in [
            "archive:../etc/1",
            "archive:/etc/passwd",
            "archive:host-a/0",
            "archive:host-a/1x",
            "memory:host-a/1",
        ] {
            assert_eq!(
                kind(m.resolve_archive_ref(bad, &trusted)),
                MapErrorKind::InvalidInput,
                "{bad}"
            );
        }
        // One install, one path; one path, one install.
        let held = ledger_len(&m);
        let moved = ArchiveSource {
            install: "host-a",
            path: &pb,
        };
        assert_eq!(
            kind(m.ingest_archive_metadata(&moved, "sp", true, 10)),
            MapErrorKind::Conflict
        );
        let renamed = ArchiveSource {
            install: "host-z",
            path: &pa,
        };
        assert_eq!(
            kind(m.ingest_archive_metadata(&renamed, "sp", true, 10)),
            MapErrorKind::Conflict
        );
        assert_eq!(
            kind(m.ingest_archive_metadata(
                &ArchiveSource {
                    install: "Host A",
                    path: &pa
                },
                "sp",
                true,
                10
            )),
            MapErrorKind::InvalidInput
        );
        // Replacing the file under the same install is still refused until reset.
        let tmp = pa.with_extension("new");
        fs::copy(&pa, &tmp).unwrap();
        fs::rename(&tmp, &pa).unwrap();
        assert_eq!(
            kind(m.ingest_archive_metadata(&a, "sp", true, 10)),
            MapErrorKind::Conflict
        );
        assert_eq!(ledger_len(&m), held);
        assert!(!contains(&file_bytes(&f.map_path("m")), "/a/proj/src"));
        assert!(!contains(&file_bytes(&f.map_path("m")), TEXT_CANARY));
    }

    #[test]
    fn a_budget_interruption_anywhere_keeps_the_batch_and_cursor_unchanged() {
        let f = fx("budget");
        history(&f);
        let src = f.coord_path();
        let before = file_bytes(&src);
        let mut m = f.map("m", M);
        let total = source_ids(&src, "sp").len();
        let zero = IngestBudget {
            max_runtime: Duration::ZERO,
            max_vm_steps: u64::MAX,
        };
        assert_eq!(
            kind(m.ingest_coordinator_within(&src, "sp", 100, zero)),
            MapErrorKind::Unavailable
        );
        // Step budgets 0, 1, 2, 4, … stop the batch at every stage (source checks,
        // source query, map appends and projection) until one is large enough.
        let mut failures = 0;
        let mut steps = 0;
        let report = loop {
            let budget = IngestBudget {
                max_runtime: Duration::from_secs(60),
                max_vm_steps: steps,
            };
            match m.ingest_coordinator_within(&src, "sp", 100, budget) {
                Ok(r) => break r,
                Err(e) => {
                    assert_eq!(e.kind, MapErrorKind::Unavailable, "{e}");
                    assert!(e.message.contains("budget"), "{e}");
                    assert_eq!(
                        (
                            ledger_len(&m),
                            count(&m, "ingest_cursors"),
                            count(&m, "ingest_seen")
                        ),
                        (0, 0, 0)
                    );
                    failures += 1;
                    steps = if steps == 0 { 1 } else { steps * 2 };
                    assert!(steps < 1 << 30, "never finished");
                }
            }
        };
        assert!(failures >= 4, "only {failures} interruptions");
        assert_eq!((report.appended, report.caught_up), (total, true));
        assert!(before == file_bytes(&src), "the source changed");
        // The store is usable afterwards, and the archive path is bounded the same way.
        assert_eq!(m.ingest_coordinator(&src, "sp", 100).unwrap().appended, 0);
        declare(&mut m, "proj", "/a/proj", true);
        let pa = archive(
            &f,
            "arch",
            ARCHIVE_SCHEMA,
            true,
            &[(1, Some("s1"), Some("/a/proj"))],
        );
        let tight = IngestBudget {
            max_runtime: Duration::from_secs(60),
            max_vm_steps: 0,
        };
        let a = ArchiveSource {
            install: "inst-a",
            path: &pa,
        };
        let held = ledger_len(&m);
        assert_eq!(
            kind(m.ingest_archive_metadata_within(&a, "sp", true, 10, tight)),
            MapErrorKind::Unavailable
        );
        assert_eq!((ledger_len(&m), count(&m, "archive_installs")), (held, 0));
        assert_eq!(
            m.ingest_archive_metadata(&a, "sp", true, 10)
                .unwrap()
                .appended,
            1
        );
    }

    #[test]
    fn a_context_packet_carries_archive_refs_but_never_archive_text_or_cwd() {
        let f = fx("packet");
        let mut m = f.map("m", M);
        declare(&mut m, "proj", "/a/proj", true);
        let pa = archive(
            &f,
            "arch",
            ARCHIVE_SCHEMA,
            true,
            &[
                (1, Some("s1"), Some("/a/proj/src")),
                (2, Some("s2"), Some("/a/proj")),
            ],
        );
        ingest_arch(&mut m, &pa, "sp", true, 10).unwrap();
        let input = crate::map::PacketInput {
            version: crate::map::packet::PACKET_INPUT_VERSION,
            install: "inst-a".into(),
            space: "sp".into(),
            project: Some("proj".into()),
            task: None,
            tier: crate::map::Tier::Strong,
            budget_bytes: None,
            open_tasks: Vec::new(),
            task_projects: Vec::new(),
            memories: Vec::new(),
        };
        let p = m.context_packet(&input).unwrap();
        for r in [
            "archive:inst-a/1",
            "archive:inst-a/2",
            "archive-session:codex/s1",
            "archive-session:claude/s2",
            "project:sp/proj",
        ] {
            assert!(p.json.contains(&format!("\"{r}\"")), "{r} missing");
        }
        // The declared root is map data an agent asserted; only archive cwds must stay out.
        for secret in [TEXT_CANARY, "/a/proj/src"] {
            assert!(!p.json.contains(secret), "{secret} leaked into the packet");
        }
        assert_eq!(
            m.resolve_archive_ref(
                "archive:inst-a/2",
                &[ArchiveSource {
                    install: "inst-a",
                    path: &pa
                }]
            )
            .unwrap(),
            (pa.clone(), 2)
        );
    }
}
