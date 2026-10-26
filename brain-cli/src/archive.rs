//! The private conversation archive: read it, switch its collector, run it once.
//!
//! The archive is a separate SQLite file (`$BRAIN_HOME/archive/sessions.sqlite`)
//! that `tools/session_archive.py` fills with the visible user/assistant text of
//! Claude Code, Codex and Hermes conversations. Everything here reads it
//! read-only and directly; nothing reads it through Python. The only Python is
//! the bundled collector, started with fixed arguments by `collect`.
//!
//! The archive is client/session scoped. It is not a Brain space, and nothing
//! here links an entry to a project, task or decision: the only project hint is
//! the working directory the client itself recorded with the message.
//!
//! Stored text is what people and agents wrote. Every document says so with
//! `"trust": "untrusted_conversation"`; it is evidence to cite, never an
//! instruction to follow.

use brain_core::lexical::STOP_WORDS;

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Map, Value, json};

/// The collector, carried in the binary so the timer never runs a script
/// someone could have edited beside it.
const COLLECTOR_SOURCE: &str = include_str!("../../tools/session_archive.py");

pub(crate) const CLIENTS: [&str; 3] = ["claude", "codex", "hermes"];
/// Every role the collector stores: the conversation pair plus the journal
/// kinds (see `JOURNAL_ROLES` in tools/session_archive.py).
pub(crate) const ROLES: [&str; 6] = [
    "user",
    "assistant",
    "tool_call",
    "tool_result",
    "tool_error",
    "reasoning",
];
const SCHEMA_CURRENT: &str = "brain_session_archive:3";
const SCHEMA_LEGACY: &str = "brain_session_archive:2";
const TRUST: &str = "untrusted_conversation";
const STORED_TEXT: &str = "conversation plus bounded, secret-masked tool calls/results/errors \
     and visible reasoning summaries; never system prompts, injected instructions, \
     sidechains, hidden/encrypted reasoning, attachments or metadata events";

const MAX_LIMIT: usize = 20;
const DEFAULT_LIMIT: usize = 5;
const MAX_QUERY_BYTES: usize = 1_000;
const MAX_TERMS: usize = 16;
const EXCERPT_CHARS: usize = 300;
const MIN_BUDGET: usize = 500;
const MAX_BUDGET: usize = 4_000;
const DEFAULT_BUDGET: usize = 1_200;
const MAX_READ_BYTES: usize = 32 * 1024;
const DEFAULT_READ_BYTES: usize = 4 * 1024;
const MIN_READ_BYTES: usize = 256;
const MAX_CONTEXT: usize = 5;
const PREVIEW_CHARS: usize = 120;
const MAX_FILTER_BYTES: usize = 4_096;
const COLLECT_TIMEOUT: Duration = Duration::from_secs(150);
const MAX_HELPER_OUTPUT: u64 = 1 << 20;
const SINCE_DAYS_RANGE: (u64, u64) = (1, 3_650);
/// Clients the collector has an adapter for; the only names `clients` may hold.
const COLLECTOR_CLIENTS: [&str; 3] = ["claude", "codex", "hermes"];
const PYTHONS: [&str; 2] = ["/usr/bin/python3", "/usr/local/bin/python3"];
pub(crate) const TIMER_UNIT: &str = "brain-archive-collect.timer";
pub(crate) const SERVICE_UNIT: &str = "brain-archive-collect.service";

/// Where the archive and its collector live under one Brain home.
pub(crate) struct Paths {
    dir: PathBuf,
    database: PathBuf,
    config: PathBuf,
    backfill: PathBuf,
    helper: PathBuf,
}

impl Paths {
    pub(crate) fn new(brain_home: &Path) -> Self {
        let dir = brain_home.join("archive");
        Self {
            database: dir.join("sessions.sqlite"),
            config: dir.join("collector.json"),
            backfill: dir.join("backfill.json"),
            helper: dir.join("collector.py"),
            dir,
        }
    }
}

/// Why the archive could not answer.
#[derive(Debug)]
pub(crate) enum ArchiveError {
    /// No archive file yet: nothing has been collected or imported.
    NotConfigured,
    /// The file exists but is not safe to read as ours (owner, mode, symlink).
    Unsafe(String),
    /// The file is not an archive this binary understands.
    Incompatible(String),
    BadRequest(String),
    NotFound(String),
    Database(rusqlite::Error),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => formatter.write_str(
                "conversation archive is not configured yet (no archive file); run `brain archive enable` then `brain archive collect`",
            ),
            Self::Unsafe(message) | Self::Incompatible(message) | Self::BadRequest(message)
            | Self::NotFound(message) => formatter.write_str(message),
            Self::Database(error) => write!(formatter, "archive database error: {error}"),
        }
    }
}

impl Error for ArchiveError {}

impl From<rusqlite::Error> for ArchiveError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl ArchiveError {
    /// Machine-readable kind for MCP and HTTP callers.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Unsafe(_) => "unsafe_permissions",
            Self::Incompatible(_) => "incompatible_archive",
            Self::BadRequest(_) => "bad_request",
            Self::NotFound(_) => "not_found",
            Self::Database(_) => "database_error",
        }
    }
}

pub(crate) struct Archive {
    pub(crate) connection: Connection,
    version: u8,
    /// The collector keeps a `quarantine` table of stored rows it no longer counts as
    /// conversation text (e.g. tool output Codex flattened into an imported session).
    quarantine: bool,
}

fn current_uid() -> Result<u32, ArchiveError> {
    // /proc/self belongs to the effective user of this process.
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|_| ArchiveError::Unsafe("cannot determine the current user; refusing".into()))
}

/// Opens the archive read-only after checking it is ours and really an archive.
pub(crate) fn open(database: &Path) -> Result<Archive, ArchiveError> {
    let metadata = match fs::symlink_metadata(database) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ArchiveError::NotConfigured);
        }
        Err(error) => {
            return Err(ArchiveError::Unsafe(format!(
                "archive file is unreadable: {}",
                error.kind()
            )));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArchiveError::Unsafe(
            "archive path is a symlink or not a regular file; refusing".into(),
        ));
    }
    let uid = current_uid()?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(ArchiveError::Unsafe(
            "archive file must be owned by you with owner-only permissions (chmod 600); refusing"
                .into(),
        ));
    }
    // A directory others can write to would let them swap the file between
    // these checks and the open below; side files must be ours as well.
    let directory = database
        .parent()
        .and_then(|parent| fs::metadata(parent).ok());
    if directory.is_none_or(|meta| meta.uid() != uid || meta.mode() & 0o022 != 0) {
        return Err(ArchiveError::Unsafe(
            "archive directory must be yours and not group/other writable; refusing".into(),
        ));
    }
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut side = database.as_os_str().to_owned();
        side.push(suffix);
        if fs::symlink_metadata(&side).is_ok_and(|meta| {
            meta.file_type().is_symlink() || meta.uid() != uid || meta.mode() & 0o022 != 0
        }) {
            return Err(ArchiveError::Unsafe(
                "archive side file is a symlink, foreign or writable by others; refusing".into(),
            ));
        }
    }
    let mut header = [0_u8; 16];
    let readable = fs::File::open(database).and_then(|mut file| file.read_exact(&mut header));
    if readable.is_err() || &header != b"SQLite format 3\0" {
        return Err(ArchiveError::Incompatible(
            "archive file is not an SQLite database".into(),
        ));
    }
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let version = check_schema(&connection)?;
    let quarantine = check_quarantine(&connection)?;
    Ok(Archive {
        connection,
        version,
        quarantine,
    })
}

impl Archive {
    /// SQL condition: entry `{alias}` is not in active quarantine.
    pub(crate) fn visible(&self, alias: &str) -> String {
        if self.quarantine {
            format!(
                "NOT EXISTS (SELECT 1 FROM quarantine q WHERE q.entry_id = {alias}.id AND q.released_at IS NULL)"
            )
        } else {
            "1".into()
        }
    }
}

/// The quarantine table is optional; when present it must have the columns we filter on.
fn check_quarantine(connection: &Connection) -> Result<bool, ArchiveError> {
    let present = columns(connection, "quarantine")?;
    if present.is_empty() {
        return Ok(false);
    }
    for column in ["entry_id", "reason", "released_at"] {
        if !present.iter().any(|name| name == column) {
            return Err(ArchiveError::Incompatible(format!(
                "archive table quarantine lacks column {column}; refusing"
            )));
        }
    }
    Ok(true)
}

fn columns(connection: &Connection, table: &str) -> Result<Vec<String>, ArchiveError> {
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info(?1)")?;
    let names = statement
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

/// Accepts schema 2 and 3 with every column this module reads; anything else
/// — a Brain database, a stranger's file, a future version — is refused.
fn check_schema(connection: &Connection) -> Result<u8, ArchiveError> {
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view')")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(|_| {
            ArchiveError::Incompatible("archive file is not a readable SQLite database".into())
        })?;
    for table in ["archive_meta", "sources", "entries", "entries_fts"] {
        if !tables.iter().any(|name| name == table) {
            return Err(ArchiveError::Incompatible(
                "unknown database schema; this is not a conversation archive".into(),
            ));
        }
    }
    let marker: Option<String> = connection
        .query_row(
            "SELECT value FROM archive_meta WHERE key = 'schema'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let version = match marker.as_deref() {
        Some(SCHEMA_CURRENT) => 3,
        Some(SCHEMA_LEGACY) => 2,
        _ => {
            return Err(ArchiveError::Incompatible(
                "unknown archive schema version; refusing".into(),
            ));
        }
    };
    let required: &[(&str, &[&str])] = &[
        ("sources", &["id", "client", "path_sha256"]),
        (
            "entries",
            &[
                "id",
                "source_id",
                "session_id",
                "role",
                "locator",
                "source_offset",
                "ts",
                "content",
                "content_sha256",
                "evidence_sha256",
                "record_sha256",
                "imported_at",
            ],
        ),
    ];
    for (table, needed) in required {
        let present = columns(connection, table)?;
        let mut needed = needed.to_vec();
        if version == 3 && *table == "entries" {
            needed.push("cwd");
        }
        if let Some(missing) = needed
            .iter()
            .find(|column| !present.iter().any(|name| name == *column))
        {
            return Err(ArchiveError::Incompatible(format!(
                "archive table {table} lacks column {missing}; refusing"
            )));
        }
    }
    if version == 3 {
        for table in ["cursors", "collector_runs"] {
            if !tables.iter().any(|name| name == table) {
                return Err(ArchiveError::Incompatible(format!(
                    "archive lacks table {table}; refusing"
                )));
            }
        }
    }
    Ok(version)
}

// ------------------------------------------------------------------ status

/// What is configured, what the collector last observed, what is stored.
pub(crate) fn status(brain_home: &Path, home: &Path) -> Value {
    let paths = Paths::new(brain_home);
    let configured = json!({
        "collector_config": config_document(&paths),
        "timer": timer_document(home),
    });
    let archive = open(&paths.database);
    let mut document = json!({
        "trust": TRUST,
        "scope": "per client and session; not a Brain space, not linked to projects or tasks",
        "archive_location": "$BRAIN_HOME/archive/sessions.sqlite",
        "complete_capture_claimed": false,
        "stored_text": STORED_TEXT,
        "configured": configured,
    });
    let object = document.as_object_mut().expect("object literal");
    match archive.and_then(|archive| stored_document(&archive).map(|stored| (archive, stored))) {
        Ok((archive, stored)) => {
            object.insert("status".into(), json!("ok"));
            object.insert("schema".into(), json!(archive.version));
            object.insert("stored".into(), stored);
            object.insert(
                "observed".into(),
                observed_document(&archive).unwrap_or(Value::Null),
            );
        }
        Err(error) => {
            object.insert(
                "status".into(),
                json!(if matches!(error, ArchiveError::NotConfigured) {
                    "not_configured"
                } else {
                    "error"
                }),
            );
            object.insert("error_kind".into(), json!(error.kind()));
            object.insert("error".into(), json!(error.to_string()));
        }
    }
    object.insert(
        "coverage_note".into(),
        json!(
            "Counts describe what the collector has seen and stored. Sources older than \
               since_days are counted but not collected; a full history is never claimed."
        ),
    );
    document
}

/// Hermes profile label for a source path: the profile name for a
/// `profiles/<name>/state.db` database, `default` for everything else.
///
/// Mirrors the collector's session namespace (`profiles/<name>/<id>`, see
/// session_archive.py): the path shape decides, not the home it lives in, so
/// a relocated home changes neither the label nor the namespace.
fn hermes_profile_label(path: &str) -> String {
    let source = Path::new(path);
    let in_profiles = source
        .parent()
        .and_then(|parent| parent.parent())
        .and_then(|profiles| profiles.file_name())
        .is_some_and(|name| name == "profiles");
    match (
        source.file_name(),
        in_profiles,
        source.parent().and_then(|parent| parent.file_name()),
    ) {
        (Some(file), true, Some(profile))
            if file == "state.db" && profile.to_str().is_some_and(valid_profile_name) =>
        {
            profile.to_string_lossy().into_owned()
        }
        _ => "default".to_owned(),
    }
}

/// Profile names the collector accepts (see `PROFILE_NAME` in session_archive.py):
/// one plain ASCII lead character, then letters, digits, dots, underscores, dashes.
fn valid_profile_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|lead| lead.is_ascii_alphanumeric())
        && characters.all(|rest| rest.is_ascii_alphanumeric() || matches!(rest, '.' | '_' | '-'))
}

fn stored_document(archive: &Archive) -> Result<Value, ArchiveError> {
    let connection = &archive.connection;
    let visible = archive.visible("e");
    let mut by_client = Map::new();
    let mut statement = connection.prepare(&format!(
        "SELECT s.client, count(*), count(DISTINCT e.session_id), count(DISTINCT e.source_id),
                max(e.imported_at), min(e.imported_at)
         FROM entries e JOIN sources s ON s.id = e.source_id WHERE {visible}
         GROUP BY s.client ORDER BY s.client"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    for row in rows {
        let (client, entries, sessions, sources, last, first) = row?;
        by_client.insert(
            client,
            json!({"entries": entries, "sessions": sessions, "sources": sources,
                   "first_observed_at": first, "last_observed_at": last}),
        );
    }
    // Hermes counts per profile as well: the top-level database is "default"
    // and every profiles/<name>/state.db is its own source whose sessions are
    // stored namespaced as profiles/<name>/<id>, so ids never collide.
    let mut hermes_profiles: std::collections::BTreeMap<String, (i64, i64)> =
        std::collections::BTreeMap::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT s.path, count(*), count(DISTINCT e.source_id)
             FROM entries e JOIN sources s ON s.id = e.source_id
             WHERE {visible} AND s.client = 'hermes'
             GROUP BY s.path ORDER BY s.path"
        ))?;
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })? {
            let (path, entries, sources) = row?;
            let per_profile = hermes_profiles
                .entry(hermes_profile_label(&path))
                .or_insert((0, 0));
            per_profile.0 += entries;
            per_profile.1 += sources;
        }
    }
    // Sessions are counted per label, not summed per source: the same raw id
    // may legitimately sit under two source paths.
    let mut session_ids: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    {
        let mut statement = connection.prepare(&format!(
            "SELECT DISTINCT s.path, e.session_id FROM entries e
             JOIN sources s ON s.id = e.source_id
             WHERE {visible} AND s.client = 'hermes' AND e.session_id IS NOT NULL"
        ))?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (path, session) = row?;
            session_ids.insert((hermes_profile_label(&path), session));
        }
    }
    let mut by_hermes_profile = Map::new();
    for (label, (entries, sources)) in hermes_profiles {
        let sessions = session_ids
            .iter()
            .filter(|(profile, _)| *profile == label)
            .count();
        by_hermes_profile.insert(
            label,
            json!({"entries": entries, "sessions": sessions, "sources": sources}),
        );
    }
    let mut roles = Map::new();
    let mut statement = connection.prepare(&format!(
        "SELECT e.role, count(*) FROM entries e WHERE {visible} GROUP BY e.role ORDER BY e.role"
    ))?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })? {
        let (role, count) = row?;
        roles.insert(role, json!(count));
    }
    let entries: i64 = connection.query_row(
        &format!("SELECT count(*) FROM entries e WHERE {visible}"),
        [],
        |row| row.get(0),
    )?;
    let mut quarantined = Map::new();
    let mut quarantine_clients = Map::new();
    if archive.quarantine {
        let mut statement = connection.prepare(
            "SELECT reason, count(*) FROM quarantine WHERE released_at IS NULL GROUP BY reason ORDER BY reason",
        )?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (reason, count) = row?;
            quarantined.insert(reason, json!(count));
        }
    }
    if archive.quarantine {
        let mut statement = connection.prepare(
            "SELECT s.client, count(DISTINCT e.id) FROM quarantine q
             JOIN entries e ON e.id = q.entry_id JOIN sources s ON s.id = e.source_id
             WHERE q.released_at IS NULL GROUP BY s.client",
        )?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })? {
            let (client, count) = row?;
            quarantine_clients.insert(client, json!(count));
        }
    }
    Ok(
        json!({"entries": entries, "by_client": by_client, "by_role": roles,
              "hermes_profiles": by_hermes_profile,
              "quarantined": {"by_reason": quarantined, "by_client": quarantine_clients,
                              "note": "kept in the archive but excluded from search, read and these counts"}}),
    )
}

/// The collector's own view: cursor states now and the last run's counts.
fn observed_document(archive: &Archive) -> Result<Value, ArchiveError> {
    if archive.version < 3 {
        return Ok(
            json!({"collector_ran": false, "note": "schema 2 archive: imported by hand, no collector cursors"}),
        );
    }
    let connection = &archive.connection;
    let mut cursors = Map::new();
    let mut errors = Vec::new();
    let mut statement = connection.prepare(
        "SELECT client, state, count(*), sum(resets), max(last_visit) FROM cursors GROUP BY client, state",
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })? {
        let (client, state, count, resets) = row?;
        let entry = cursors
            .entry(client)
            .or_insert_with(|| json!({"sources_by_state": {}, "resets": 0}));
        entry["sources_by_state"][state] = json!(count);
        let total = entry["resets"].as_i64().unwrap_or(0) + resets.unwrap_or(0);
        entry["resets"] = json!(total);
    }
    let mut statement = connection.prepare(
        "SELECT client, last_error, count(*) FROM cursors WHERE state = 'error'
         GROUP BY client, last_error ORDER BY client LIMIT 20",
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })? {
        let (client, error, count) = row?;
        errors.push(json!({"client": client, "error": error, "sources": count}));
    }
    let last_run = connection
        .query_row(
            "SELECT started_at, finished_at, outcome, summary FROM collector_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let runs: i64 =
        connection.query_row("SELECT count(*) FROM collector_runs", [], |row| row.get(0))?;
    let last_run = last_run.map(|(started, finished, outcome, summary)| {
        let summary: Value = serde_json::from_str(&summary).unwrap_or(Value::Null);
        let mut clients = Map::new();
        if let Some(per_client) = summary.get("clients").and_then(Value::as_object) {
            for (client, counts) in per_client {
                let keep = [
                    "root",
                    "discovered",
                    "in_scope",
                    "out_of_window",
                    "missing",
                    "visited",
                    "inserted",
                    "duplicates",
                    "resets",
                    "bytes_in_scope",
                    "bytes_covered",
                    "sources_by_state",
                    "discovery_truncated",
                ];
                let picked: Map<String, Value> = keep
                    .iter()
                    .filter_map(|key| {
                        counts
                            .get(*key)
                            .map(|value| ((*key).to_owned(), value.clone()))
                    })
                    .collect();
                clients.insert(client.clone(), Value::Object(picked));
            }
        }
        json!({"started_at": started, "finished_at": finished, "outcome": outcome,
               "stopped_by": summary.get("stopped_by"), "visited": summary.get("visited"),
               "elapsed_seconds": summary.get("elapsed_seconds"),
               "scope": summary.get("scope"), "budgets": summary.get("budgets"),
               "clients": clients})
    });
    Ok(
        json!({"collector_ran": runs > 0, "runs_recorded": runs, "cursors": cursors,
              "errors": errors, "last_run": last_run}),
    )
}

/// The collector switch as one word, from the config file alone (no database).
pub(crate) fn collector_switch(brain_home: &Path) -> &'static str {
    match read_config(&Paths::new(brain_home)) {
        Ok(None) => "archive_not_configured",
        Ok(Some(config)) if config.get("enabled") == Some(&json!(true)) => {
            "archive_collector_enabled"
        }
        Ok(Some(_)) => "archive_collector_paused",
        Err(_) => "archive_config_invalid",
    }
}

fn config_document(paths: &Paths) -> Value {
    match read_config(paths) {
        Ok(None) => json!({"present": false, "enabled": false, "since_days": 14,
            "clients_configured": COLLECTOR_CLIENTS, "roles_configured": ROLES}),
        Ok(Some(config)) => {
            // Configured consent, not observed coverage: which clients were really
            // read is only in the last collector run.
            let (clients, selection) = match config.get("clients") {
                None => (json!(COLLECTOR_CLIENTS), "legacy_all"),
                Some(value) => match validate_clients(value) {
                    Ok(clients) => (json!(clients), "explicit"),
                    Err(error) => {
                        return json!({"present": true, "enabled": false,
                                      "error": error.to_string()});
                    }
                },
            };
            let roles = match config.get("roles") {
                None => json!(ROLES),
                Some(value) => match validate_roles(value) {
                    Ok(roles) => json!(roles),
                    Err(error) => {
                        return json!({"present": true, "enabled": false,
                                                "error": error.to_string()});
                    }
                },
            };
            json!({
                "present": true,
                "enabled": config.get("enabled").and_then(Value::as_bool).unwrap_or(false),
                "since_days": config.get("since_days").cloned().unwrap_or(json!(14)),
                "clients_configured": clients,
                "client_selection": selection,
                "roles_configured": roles,
                "role_selection": if config.get("roles").is_some() { "explicit" } else { "legacy_all" },
            })
        }
        Err(error) => json!({"present": true, "enabled": false, "error": error.to_string()}),
    }
}

fn timer_document(home: &Path) -> Value {
    let units = home.join(".config/systemd/user");
    let installed = units.join(TIMER_UNIT).is_file() && units.join(SERVICE_UNIT).is_file();
    let enabled = fs::symlink_metadata(units.join("timers.target.wants").join(TIMER_UNIT)).is_ok();
    json!({"unit_files_installed": installed, "enabled_at_boot": enabled,
           "note": "installed/enabled are file facts; whether it actually ran shows in observed.last_run"})
}

// ------------------------------------------------------------------ search

/// One visible, already secret-masked archive entry for local distillation.
#[derive(Clone, Debug)]
pub(crate) struct DistillEntry {
    pub id: i64,
    pub session_id: String,
    pub ts: Option<String>,
    pub cwd: Option<String>,
    pub content: String,
    pub role: String,
}

/// Session heads use the same visibility checks as archive reads. Only metadata
/// is returned; content is retrieved in bounded batches after cursor selection.
pub(crate) fn distill_sessions(
    brain_home: &Path,
    session: Option<&str>,
    since: Option<&str>,
    roles: &[String],
) -> Result<Vec<(String, i64)>, ArchiveError> {
    open(&Paths::new(brain_home).database)?.distill_sessions(session, since, roles)
}

impl Archive {
    pub(crate) fn distill_sessions(
        &self,
        session: Option<&str>,
        since: Option<&str>,
        roles: &[String],
    ) -> Result<Vec<(String, i64)>, ArchiveError> {
        let archive = self;
        let session = bounded_filter("session_id", session)?;
        let since = since
            .map(|s| {
                epoch_seconds(s).ok_or_else(|| {
                    ArchiveError::BadRequest("since must be an ISO date or Unix timestamp".into())
                })
            })
            .transpose()?;
        let visible = archive.visible("e");
        // One metadata scan replaces a full timestamp query for every session.
        // Parse in Rust so mixed ISO/numeric timestamps retain their exact semantics.
        let sql = format!(
            "SELECT e.session_id,e.id,e.ts FROM entries e WHERE {visible}
        AND e.role IN (SELECT value FROM json_each(?2))
        AND e.session_id IS NOT NULL AND (?1 IS NULL OR e.session_id=?1)"
        );
        let mut statement = archive.connection.prepare(&sql)?;
        let mut rows = statement.query(params![session, json!(roles).to_string()])?;
        let mut heads = std::collections::BTreeMap::<String, (i64, Option<i64>)>::new();
        while let Some(row) = rows.next()? {
            let session: String = row.get(0)?;
            let id: i64 = row.get(1)?;
            let ts: Option<String> = row.get(2)?;
            let time = ts.as_deref().and_then(epoch_seconds);
            let head = heads.entry(session).or_insert((id, None));
            head.0 = head.0.max(id);
            head.1 = head.1.max(time);
        }
        let mut timed: Vec<_> = heads
            .into_iter()
            .filter_map(|(session, (head, latest))| {
                if since.is_some_and(|s| latest.is_none_or(|t| t < s)) {
                    None
                } else {
                    Some((latest, session, head))
                }
            })
            .collect();
        timed.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let result = timed
            .into_iter()
            .map(|(_, session, head)| (session, head))
            .collect();
        Ok(result)
    }

    /// Content is already masked by the collector. No raw source is opened.
    pub(crate) fn distill_entries(
        &self,
        session: &str,
        after: i64,
        roles: &[String],
    ) -> Result<Vec<DistillEntry>, ArchiveError> {
        let archive = self;
        let visible = archive.visible("e");
        let cwd = if archive.version >= 3 {
            "e.cwd"
        } else {
            "NULL"
        };
        let sql = format!(
        "SELECT e.id,e.session_id,e.ts,{cwd},e.content,e.role
        FROM sources s CROSS JOIN entries e
        WHERE e.source_id=s.id AND {visible} AND e.session_id=?1 AND e.id>?2 AND e.role IN (SELECT value FROM json_each(?3)) ORDER BY e.id LIMIT 64"
    );
        let mut statement = archive.connection.prepare(&sql)?;
        let rows = statement
            .query_map(params![session, after, json!(roles).to_string()], |row| {
                Ok(DistillEntry {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    ts: row.get(2)?,
                    cwd: row.get(3)?,
                    content: row.get(4)?,
                    role: row.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }
}

/// Filters and bounds for one search.
#[derive(Default, Debug)]
pub(crate) struct SearchRequest {
    pub(crate) query: String,
    pub(crate) client: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) project_root: Option<String>,
    pub(crate) limit: Option<usize>,
    pub(crate) budget_tokens: Option<usize>,
    /// Match messages containing any content word rather than all words.
    /// A spoken question rarely has every one of its words in one message.
    pub(crate) match_any: bool,
    /// Half-open `[from, to)` in Unix seconds; only entries said inside it match.
    pub(crate) period: Option<(i64, i64)>,
}

/// Each whitespace-separated term becomes a quoted FTS5 phrase, so operators,
/// column filters and quotes in the query are matched as text, never parsed.
fn fts_query(query: &str) -> Result<String, ArchiveError> {
    let terms: Vec<String> = query_terms(query)?
        .take(MAX_TERMS)
        .map(quote_term)
        .collect();
    if terms.is_empty() {
        return Err(ArchiveError::BadRequest("empty query".into()));
    }
    Ok(terms.join(" "))
}

/// OR of the question's content words, ranked by bm25 like the strict form.
/// Long words become a quoted five-letter prefix query because the tokenizer
/// does no stemming: "октября" must still reach "октябре", "отказались" "отказ".
fn fts_query_any(query: &str) -> Result<String, ArchiveError> {
    let mut terms: Vec<String> = Vec::new();
    for raw in query_terms(query)? {
        let term = raw.trim_matches(|c: char| !c.is_alphanumeric());
        let lower = term.to_lowercase();
        let chars = lower.chars().count();
        let has_digit = lower.chars().any(|c| c.is_ascii_digit());
        if lower.is_empty() || (!has_digit && (chars < 3 || STOP_WORDS.contains(&lower.as_str()))) {
            continue;
        }
        let quoted = if !has_digit && chars >= 7 {
            let stem: String = lower.chars().take(5).collect();
            format!("{}*", quote_term(&stem))
        } else {
            quote_term(&lower)
        };
        if !terms.contains(&quoted) {
            terms.push(quoted);
        }
        if terms.len() == MAX_TERMS {
            break;
        }
    }
    if terms.is_empty() {
        // An OR question containing only grammar has no lexical evidence.
        // Keep strict archive phrase search separate and unchanged.
        return Ok(String::new());
    }
    Ok(terms.join(" OR "))
}

fn query_terms(query: &str) -> Result<std::str::SplitWhitespace<'_>, ArchiveError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(ArchiveError::BadRequest(format!(
            "query longer than {MAX_QUERY_BYTES} bytes"
        )));
    }
    Ok(query.split_whitespace())
}

fn quote_term(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

fn bounded_filter(name: &str, value: Option<&str>) -> Result<Option<String>, ArchiveError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) if value.len() > MAX_FILTER_BYTES || value.contains('\0') => Err(
            ArchiveError::BadRequest(format!("{name} is too long or invalid")),
        ),
        Some(value) => Ok(Some(value.to_owned())),
    }
}

/// Conservative token estimate: half a token per UTF-8 byte, rounded up.
fn estimate_tokens(value: &Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len().div_ceil(2))
}

fn excerpt(text: &str, chars: usize) -> String {
    let mut out: String = text.chars().take(chars).collect();
    if text.chars().nth(chars).is_some() {
        out.push('…');
    }
    out
}

pub(crate) fn search(brain_home: &Path, request: &SearchRequest) -> Result<Value, ArchiveError> {
    let paths = Paths::new(brain_home);
    let fts = if request.match_any {
        fts_query_any(&request.query)?
    } else {
        fts_query(&request.query)?
    };
    let client = bounded_filter("client", request.client.as_deref())?;
    if let Some(client) = &client
        && !CLIENTS.contains(&client.as_str())
    {
        return Err(ArchiveError::BadRequest(
            "client must be claude, codex or hermes".into(),
        ));
    }
    let role = bounded_filter("role", request.role.as_deref())?;
    if let Some(role) = &role
        && !ROLES.contains(&role.as_str())
    {
        return Err(ArchiveError::BadRequest(format!(
            "role must be one of {}",
            ROLES.join(", ")
        )));
    }
    let session = bounded_filter("session_id", request.session_id.as_deref())?;
    let project = bounded_filter("project_root", request.project_root.as_deref())?;
    if let Some(project) = &project
        && !project.starts_with('/')
    {
        return Err(ArchiveError::BadRequest(
            "project_root must be an absolute path".into(),
        ));
    }
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(ArchiveError::BadRequest(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    let budget = request.budget_tokens.unwrap_or(DEFAULT_BUDGET);
    if !(MIN_BUDGET..=MAX_BUDGET).contains(&budget) {
        return Err(ArchiveError::BadRequest(format!(
            "budget_tokens must be between {MIN_BUDGET} and {MAX_BUDGET}"
        )));
    }
    let archive = open(&paths.database)?;
    if project.is_some() && archive.version < 3 {
        return Err(ArchiveError::BadRequest(
            "project_root needs a schema 3 archive (the working directory is not recorded in schema 2)".into(),
        ));
    }
    if fts.is_empty() {
        return Ok(search_document(Vec::new(), limit, budget, 0, false));
    }
    let cwd = if archive.version >= 3 {
        "e.cwd"
    } else {
        "NULL"
    };
    let visible = archive.visible("e");
    let project_prefix = project.as_ref().map(|root| {
        let trimmed = root.trim_end_matches('/');
        format!("{}/", if trimmed.is_empty() { "" } else { trimmed })
    });
    // Claude and Codex record ISO-8601 times, Hermes Unix seconds as text.
    let said_at = "(CASE WHEN e.ts GLOB '[0-9]*' AND e.ts NOT GLOB '*[^0-9.]*' \
                   THEN CAST(e.ts AS REAL) ELSE CAST(strftime('%s', e.ts) AS REAL) END)";
    let sql = format!(
        "SELECT e.id, s.client, e.role, e.session_id, e.ts, e.imported_at, {cwd},
                snippet(entries_fts, 0, '', '', '…', 32)
         FROM entries_fts JOIN entries e ON e.id = entries_fts.rowid
         JOIN sources s ON s.id = e.source_id
         WHERE entries_fts MATCH ?1 AND {visible}
           AND (?2 IS NULL OR s.client = ?2)
           AND (?3 IS NULL OR e.role = ?3)
           AND (?4 IS NULL OR e.session_id = ?4)
           AND (?5 IS NULL OR {cwd} = ?5 OR substr({cwd}, 1, length(?6)) = ?6)
           AND (?8 IS NULL OR {said_at} >= ?8) AND (?9 IS NULL OR {said_at} < ?9)
         ORDER BY bm25(entries_fts), e.id LIMIT ?7"
    );
    let mut statement = archive.connection.prepare(&sql)?;
    let rows = statement
        .query_map(
            params![
                fts,
                client,
                role,
                session,
                project.as_ref().map(|root| root.trim_end_matches('/').to_owned()),
                project_prefix,
                limit as i64,
                request.period.map(|period| period.0),
                request.period.map(|period| period.1)
            ],
            |row| {
                // `timestamp` stays exactly as the client wrote it; `said_at`
                // is the same moment as readable UTC, null when unreadable.
                let timestamp: Option<String> = row.get(4)?;
                let said_at = timestamp
                    .as_deref()
                    .and_then(epoch_seconds)
                    .map(crate::stats::utc_second);
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "client": row.get::<_, String>(1)?,
                    "role": row.get::<_, String>(2)?,
                    "session_id": row.get::<_, Option<String>>(3)?,
                    "timestamp": timestamp,
                    "said_at": said_at,
                    "observed_at": row.get::<_, String>(5)?,
                    "cwd": row.get::<_, Option<String>>(6)?,
                    "excerpt": excerpt(&row.get::<_, Option<String>>(7)?.unwrap_or_default(), EXCERPT_CHARS),
                }))
            },
        )
        .map_err(|_| ArchiveError::BadRequest("query could not be evaluated".into()))?;
    let mut results = Vec::new();
    let mut used = 0_usize;
    let mut truncated_by_budget = false;
    for row in rows {
        let row =
            row.map_err(|_| ArchiveError::BadRequest("query could not be evaluated".into()))?;
        let cost = estimate_tokens(&row);
        if used + cost > budget {
            truncated_by_budget = true;
            break;
        }
        used += cost;
        results.push(row);
    }
    Ok(search_document(
        results,
        limit,
        budget,
        used,
        truncated_by_budget,
    ))
}

fn search_document(
    results: Vec<Value>,
    limit: usize,
    budget: usize,
    used: usize,
    truncated_by_budget: bool,
) -> Value {
    json!({
        "trust": TRUST,
        "scope": "conversation archive (per client/session), not a Brain space",
        "limit": limit,
        "budget_tokens": budget,
        "tokens_used_estimate": used,
        "token_estimator": "utf8-bytes-div-2",
        "truncated_by_budget": truncated_by_budget,
        "results": results,
        "next": "read one hit with archive_read {id}; excerpts are cut to 300 characters",
    })
}

// ------------------------------------------------------------------ read

#[derive(Default, Debug)]
pub(crate) struct ReadRequest {
    pub(crate) id: String,
    pub(crate) offset: Option<usize>,
    pub(crate) max_bytes: Option<usize>,
    pub(crate) context: Option<usize>,
}

fn parse_id(raw: &str) -> Result<i64, ArchiveError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 19 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ArchiveError::BadRequest(
            "id must be a positive integer from archive search".into(),
        ));
    }
    raw.parse::<i64>().ok().filter(|id| *id > 0).ok_or_else(|| {
        ArchiveError::BadRequest("id must be a positive integer from archive search".into())
    })
}

/// Seconds since the epoch for an ISO-8601 or numeric (Hermes) timestamp.
pub(crate) fn epoch_seconds(text: &str) -> Option<i64> {
    if let Ok(number) = text.parse::<f64>() {
        return number.is_finite().then_some(number as i64);
    }
    crate::mcp::parse_iso8601(text)
}

/// Largest char boundary at or below `index`.
fn floor_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) fn read(brain_home: &Path, request: &ReadRequest) -> Result<Value, ArchiveError> {
    let paths = Paths::new(brain_home);
    let id = parse_id(&request.id)?;
    let max_bytes = request.max_bytes.unwrap_or(DEFAULT_READ_BYTES);
    if !(MIN_READ_BYTES..=MAX_READ_BYTES).contains(&max_bytes) {
        return Err(ArchiveError::BadRequest(format!(
            "max_bytes must be between {MIN_READ_BYTES} and {MAX_READ_BYTES}"
        )));
    }
    let context = request.context.unwrap_or(0);
    if context > MAX_CONTEXT {
        return Err(ArchiveError::BadRequest(format!(
            "context must be between 0 and {MAX_CONTEXT}"
        )));
    }
    let archive = open(&paths.database)?;
    let cwd = if archive.version >= 3 {
        "e.cwd"
    } else {
        "NULL"
    };
    let row = archive
        .connection
        .query_row(
            &format!(
                "SELECT e.id, s.client, e.role, e.session_id, e.ts, e.imported_at, {cwd}, e.locator,
                        e.source_offset, e.content, e.content_sha256, e.evidence_sha256,
                        e.record_sha256, s.path_sha256, e.source_id
                 FROM entries e JOIN sources s ON s.id = e.source_id WHERE e.id = ?1 AND {visible}",
                visible = archive.visible("e")
            ),
            [id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, i64>(14)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| ArchiveError::NotFound("archive entry not found".into()))?;
    let (
        id,
        client,
        role,
        session,
        ts,
        observed,
        cwd_value,
        locator,
        offset,
        content,
        content_sha,
        evidence_sha,
        record_sha,
        path_sha,
        source_id,
    ) = row;
    let total = content.len();
    let start = floor_boundary(&content, request.offset.unwrap_or(0));
    if request.offset.is_some_and(|offset| offset > total) {
        return Err(ArchiveError::BadRequest(
            "offset is past the end of the entry".into(),
        ));
    }
    let end = floor_boundary(&content, start.saturating_add(max_bytes));
    let lag = ts
        .as_deref()
        .and_then(epoch_seconds)
        .zip(crate::mcp::parse_iso8601(&observed))
        .map(|(written, seen)| seen - written);
    let neighbours = if context > 0 {
        neighbours(&archive, source_id, session.as_deref(), id, context)?
    } else {
        json!(null)
    };
    Ok(json!({
        "trust": TRUST,
        "id": id,
        "client": client,
        "role": role,
        "role_note": if role == "assistant" { "an agent's claim, not observed evidence that something worked" } else { "what the person typed" },
        "session_id": session,
        "timestamp": ts.clone(),
        "said_at": ts.as_deref().and_then(epoch_seconds).map(crate::stats::utc_second),
        "observed_at": observed,
        "capture_lag_seconds": lag,
        "cwd": cwd_value,
        "cwd_note": "working directory recorded by the client with this message; not a verified project link",
        "locator": locator,
        "source_offset": offset,
        "source_path_sha256": path_sha,
        "content": &content[start..end],
        "content_offset": start,
        "content_bytes": total,
        "next_offset": (end < total).then_some(end),
        "content_sha256": content_sha,
        "evidence_sha256": evidence_sha,
        "record_sha256": record_sha,
        "stored_text": STORED_TEXT,
        "neighbours": neighbours,
    }))
}

/// Up to `count` entries before and after in the same source and session.
fn neighbours(
    archive: &Archive,
    source_id: i64,
    session: Option<&str>,
    id: i64,
    count: usize,
) -> Result<Value, ArchiveError> {
    let query = |sql: &str| -> Result<Vec<Value>, ArchiveError> {
        let mut statement = archive.connection.prepare(sql)?;
        let rows = statement
            .query_map(params![source_id, session, id, count as i64], |row| {
                let timestamp: Option<String> = row.get(2)?;
                let said_at = timestamp
                    .as_deref()
                    .and_then(epoch_seconds)
                    .map(crate::stats::utc_second);
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "role": row.get::<_, String>(1)?,
                    "timestamp": timestamp,
                    "said_at": said_at,
                    "preview": excerpt(&row.get::<_, String>(3)?, PREVIEW_CHARS),
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    let visible = archive.visible("e");
    let mut before = query(&format!(
        "SELECT e.id, e.role, e.ts, e.content FROM entries e WHERE e.source_id = ?1 AND e.session_id IS ?2
         AND e.id < ?3 AND {visible} ORDER BY e.id DESC LIMIT ?4"
    ))?;
    before.reverse();
    let after = query(&format!(
        "SELECT e.id, e.role, e.ts, e.content FROM entries e WHERE e.source_id = ?1 AND e.session_id IS ?2
         AND e.id > ?3 AND {visible} ORDER BY e.id LIMIT ?4"
    ))?;
    Ok(
        json!({"order": "archive insertion order within one source and session",
              "before": before, "after": after}),
    )
}

// ------------------------------------------------------------------ control

fn ensure_private_dir(dir: &Path) -> Result<(), Box<dyn Error>> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                return Err("archive directory is a symlink or not a directory; refusing".into());
            }
            if metadata.uid() != current_uid()? || metadata.mode() & 0o022 != 0 {
                return Err(
                    "archive directory must be yours and not group/other writable; refusing".into(),
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::create_dir(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn read_config(paths: &Paths) -> Result<Option<Map<String, Value>>, Box<dyn Error>> {
    match fs::symlink_metadata(&paths.config) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_file() => {
            return Err("collector config is a symlink or not a regular file".into());
        }
        Ok(_) => {}
    }
    let mut text = String::new();
    fs::File::open(&paths.config)?
        .take(64 * 1024)
        .read_to_string(&mut text)?;
    match serde_json::from_str::<Value>(&text)? {
        Value::Object(map) => Ok(Some(map)),
        _ => Err("collector config is not a JSON object".into()),
    }
}

/// Same rule as the collector's `load_config`: a non-empty list of distinct known
/// clients, returned in canonical order.
fn validate_clients(value: &Value) -> Result<Vec<&'static str>, Box<dyn Error>> {
    let invalid = || -> Box<dyn Error> {
        format!(
            "clients must be a non-empty list of distinct names from: {}",
            COLLECTOR_CLIENTS.join(", ")
        )
        .into()
    };
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(invalid)?;
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        let name = item
            .as_str()
            .filter(|name| COLLECTOR_CLIENTS.contains(name));
        match name {
            Some(name) if !names.contains(&name) => names.push(name),
            _ => return Err(invalid()),
        }
    }
    Ok(COLLECTOR_CLIENTS
        .into_iter()
        .filter(|known| names.contains(known))
        .collect())
}

fn validate_roles(value: &Value) -> Result<Vec<&'static str>, Box<dyn Error>> {
    let invalid = || -> Box<dyn Error> {
        format!(
            "roles must be a non-empty list of distinct names from: {}",
            ROLES.join(", ")
        )
        .into()
    };
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(invalid)?;
    let mut names = Vec::with_capacity(items.len());
    for item in items {
        let name = item.as_str().filter(|name| ROLES.contains(name));
        match name {
            Some(name) if !names.contains(&name) => names.push(name),
            _ => return Err(invalid()),
        }
    }
    Ok(ROLES
        .into_iter()
        .filter(|known| names.contains(known))
        .collect())
}

/// Persists the switch the collector checks on every run. Budgets and an existing
/// client selection are kept; `clients`/`roles` replace selections only when given.
pub(crate) fn set_enabled(
    brain_home: &Path,
    enabled: bool,
    since_days: Option<u64>,
    clients: Option<&[String]>,
    roles: Option<&[String]>,
) -> Result<Value, Box<dyn Error>> {
    let paths = Paths::new(brain_home);
    ensure_private_dir(&paths.dir)?;
    let mut config = read_config(&paths)?.unwrap_or_default();
    let selection = clients
        .map(|names| validate_clients(&json!(names)))
        .transpose()?;
    let role_selection = roles
        .map(|names| validate_roles(&json!(names)))
        .transpose()?;
    config.insert("version".into(), json!(1));
    config.insert("enabled".into(), json!(enabled));
    if let Some(names) = selection {
        config.insert("clients".into(), json!(names));
    }
    if let Some(names) = role_selection {
        config.insert("roles".into(), json!(names));
    }
    if let Some(days) = since_days {
        if !(SINCE_DAYS_RANGE.0..=SINCE_DAYS_RANGE.1).contains(&days) {
            return Err(format!(
                "--since-days must be between {} and {}",
                SINCE_DAYS_RANGE.0, SINCE_DAYS_RANGE.1
            )
            .into());
        }
        config.insert("since_days".into(), json!(days));
    }
    let text = serde_json::to_string_pretty(&Value::Object(config.clone()))? + "\n";
    write_private(&paths.config, text.as_bytes())?;
    Ok(json!({"status": "ok", "enabled": enabled,
              "since_days": config.get("since_days").cloned().unwrap_or(json!(14)),
              "clients_configured": config.get("clients").cloned()
                  .unwrap_or_else(|| json!(COLLECTOR_CLIENTS)),
              "roles_configured": config.get("roles").cloned()
                  .unwrap_or_else(|| json!(ROLES)),
              "note": if enabled { "the next collector run (timer or `brain archive collect`) will collect" }
                      else { "paused: collector runs now exit without reading any source" }}))
}

// ------------------------------------------------------------- backfill

/// Upper bound on `files_per_run`; matches the collector's own hard cap.
pub(crate) const BACKFILL_MAX_FILES: u64 = 20;

/// The optional historic-backfill switch, from its own file alone (no database).
///
/// Missing file => disabled. A malformed, insecure, symlinked or oversized file
/// => an explicit error, never a silent repair and never a write.
pub(crate) fn backfill_status(brain_home: &Path) -> Value {
    let paths = Paths::new(brain_home);
    match read_backfill(&paths) {
        Ok(None) => json!({"present": false, "state": "disabled",
                           "note": "no backfill.json: history older than the window is not imported"}),
        Ok(Some(map)) => {
            let state = map
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("running");
            let files = map.get("files_per_run").cloned().unwrap_or(json!(0));
            json!({"present": true, "state": state, "files_per_run": files,
                   "effective": state == "running" && files.as_u64().unwrap_or(0) > 0})
        }
        Err(error) => json!({"present": true, "state": "error", "error": error.to_string()}),
    }
}

/// Reads and validates `backfill.json`: a JSON object with exactly the integer
/// `v`:1, an integer `files_per_run` in 0..=20 and a `state` of running|stopped.
/// All three fields are required, matching the Python collector.
///
/// The file is opened once with `O_NOFOLLOW | O_NONBLOCK` and the resulting
/// descriptor is validated (regular file, owner, owner-only mode, size) with
/// `metadata` on that same handle; nothing is opened by path a second time, so a
/// symlink or a FIFO is refused without blocking and a path swapped mid-check
/// cannot slip a different object past the checks.
fn read_backfill(paths: &Paths) -> Result<Option<Map<String, Value>>, ArchiveError> {
    let opened = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&paths.backfill);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return match error.raw_os_error() {
                // ELOOP: the path is a symlink, which O_NOFOLLOW refuses.
                Some(libc::ELOOP) => Err(ArchiveError::Unsafe(
                    "backfill config is a symlink; refusing".into(),
                )),
                _ => Err(ArchiveError::Unsafe(format!(
                    "backfill config is unreadable: {}",
                    error.kind()
                ))),
            };
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| ArchiveError::Unsafe(format!("backfill config is unreadable: {error}")))?;
    if !metadata.is_file() {
        return Err(ArchiveError::Unsafe(
            "backfill config is not a regular file (FIFO, socket or device); refusing".into(),
        ));
    }
    let uid = current_uid()?;
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(ArchiveError::Unsafe(
            "backfill config must be owned by you with owner-only permissions (chmod 600); refusing".into(),
        ));
    }
    const CAP: usize = 64 * 1024;
    if metadata.len() > CAP as u64 {
        return Err(ArchiveError::Unsafe(
            "backfill config is larger than 64 KiB; refusing".into(),
        ));
    }
    // Read one byte past the cap: the size check above can race with a writer, so
    // an actually-larger file must be rejected, not accepted by its valid prefix.
    let mut raw = Vec::new();
    file.take(CAP as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(|error| ArchiveError::Unsafe(format!("backfill config is unreadable: {error}")))?;
    if raw.len() > CAP {
        return Err(ArchiveError::Unsafe(
            "backfill config grew past 64 KiB while being read; refusing".into(),
        ));
    }
    let text = String::from_utf8(raw)
        .map_err(|_| ArchiveError::BadRequest("backfill config is not valid UTF-8".into()))?;
    let map = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => map,
        Ok(_) => {
            return Err(ArchiveError::BadRequest(
                "backfill config is not a JSON object".into(),
            ));
        }
        Err(_) => {
            return Err(ArchiveError::BadRequest(
                "backfill config is not valid JSON".into(),
            ));
        }
    };
    for key in map.keys() {
        if !matches!(key.as_str(), "v" | "files_per_run" | "state") {
            return Err(ArchiveError::BadRequest(format!(
                "unknown backfill config key: {key}"
            )));
        }
    }
    match map.get("v") {
        Some(Value::Number(number)) if number.as_u64() == Some(1) => {}
        _ => {
            return Err(ArchiveError::BadRequest(
                "backfill config v must be the integer 1".into(),
            ));
        }
    }
    match map.get("files_per_run") {
        Some(Value::Number(number))
            if number
                .as_u64()
                .is_some_and(|value| value <= BACKFILL_MAX_FILES) => {}
        _ => {
            return Err(ArchiveError::BadRequest(format!(
                "backfill config files_per_run must be an integer in [0, {BACKFILL_MAX_FILES}]"
            )));
        }
    }
    match map.get("state").and_then(Value::as_str) {
        Some("running" | "stopped") => {}
        _ => {
            return Err(ArchiveError::BadRequest(
                "backfill config state must be running or stopped".into(),
            ));
        }
    }
    Ok(Some(map))
}

/// Starts or restarts historic backfill with an explicit quota (1..=20).
///
/// An existing config is read first: a malformed one is reported and left
/// byte-for-byte untouched, never silently replaced or repaired.
pub(crate) fn backfill_start(
    brain_home: &Path,
    files_per_run: u64,
) -> Result<Value, Box<dyn Error>> {
    if !(1..=BACKFILL_MAX_FILES).contains(&files_per_run) {
        return Err(format!("--files-per-run must be between 1 and {BACKFILL_MAX_FILES}").into());
    }
    read_backfill(&Paths::new(brain_home))?; // refuse a broken existing config, do not overwrite
    write_backfill(brain_home, files_per_run, "running")
}

/// Stops admitting new old files; already-tracked sources keep being caught up.
pub(crate) fn backfill_stop(brain_home: &Path) -> Result<Value, Box<dyn Error>> {
    let paths = Paths::new(brain_home);
    let files = match read_backfill(&paths)? {
        Some(map) => map
            .get("files_per_run")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        None => 0,
    };
    write_backfill(brain_home, files, "stopped")
}

fn write_backfill(
    brain_home: &Path,
    files_per_run: u64,
    state: &str,
) -> Result<Value, Box<dyn Error>> {
    debug_assert!(files_per_run <= BACKFILL_MAX_FILES);
    let paths = Paths::new(brain_home);
    ensure_private_dir(&paths.dir)?;
    let text = serde_json::to_string_pretty(&json!({
        "v": 1, "files_per_run": files_per_run, "state": state
    }))? + "\n";
    write_private(&paths.backfill, text.as_bytes())?;
    Ok(json!({
    "status": "ok", "state": state, "files_per_run": files_per_run,
    "note": if state == "running" {
        "the next collector run admits up to this many older files into tracking; it never enables the collector itself"
    } else {
        "stopped: no new old files are admitted; already-tracked sources are still caught up"
    }}))
}

/// Distinct sources with at least one visible entry, per client.
///
/// This is NOT a count of historic imports: it counts every source the archive
/// has stored visible conversation from, whether the regular since_days window
/// or backfill admitted it. It is named accordingly so the UI cannot mislabel it.
fn stored_visible_sources_by_client(paths: &Paths) -> Option<Map<String, Value>> {
    let archive = open(&paths.database).ok()?;
    let visible = archive.visible("e");
    let mut counts = Map::new();
    let mut statement = archive
        .connection
        .prepare(&format!(
            "SELECT s.client, count(DISTINCT e.source_id) FROM entries e JOIN sources s ON s.id = e.source_id WHERE {visible} GROUP BY s.client"
        ))
        .ok()?;
    for (client, count) in statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()?
        .flatten()
    {
        counts.insert(client, json!(count));
    }
    Some(counts)
}

// ------------------------------------------------------------- backfill status

/// The backfill switch plus honest, database-derived coverage.
///
/// Nothing here ever claims the whole history is captured. Three distinct
/// counts, never conflated:
///  - `stored_visible_sources_by_client`: all sources with stored visible text
///    (regular window plus backfill), the read-contract count.
///  - `last_run_admitted_old_files` / `last_run_selected_old_files`: what the
///    last collector run recorded as newly tracked into historic import vs
///    merely selected for admission (may be null when no run recorded it).
///  - `remaining_out_of_window` / `watermark_mtime` / `contiguous` from the last
///    run, null when unknown. `complete_capture_claimed` stays false.
pub(crate) fn backfill_document(brain_home: &Path) -> Value {
    let paths = Paths::new(brain_home);
    let status = backfill_status(brain_home);
    let mut document = json!({
        "trust": TRUST,
        "scope": "older visible conversation files admitted a few at a time; not a Brain space and not linked to projects or tasks",
        "complete_capture_claimed": false,
        "backfill": status,
    });
    let object = document.as_object_mut().expect("object literal");
    // Configured vs observed: the collector switch and the last real run are shown
    // separately so the UI never infers an active process from the config alone.
    object.insert(
        "collector_enabled".into(),
        json!(
            config_document(&paths)
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        ),
    );
    match open(&paths.database) {
        Ok(archive) => {
            let last = last_run_summary(&archive);
            let stored = stored_visible_sources_by_client(&paths).unwrap_or_default();
            let pointer = |path: &str| {
                last.as_ref()
                    .and_then(|summary| summary.pointer(path).cloned())
            };
            object.insert("status".into(), json!("ok"));
            object.insert(
                "observed_last_run".into(),
                last.as_ref()
                    .map(|summary| {
                        json!({
                            "finished_at": summary.get("finished_at"),
                            "outcome": summary.get("outcome"),
                            "stopped_by": summary.get("stopped_by"),
                        })
                    })
                    .unwrap_or(Value::Null),
            );
            object.insert("coverage".into(), json!({
                "stored_visible_sources_by_client": stored,
                "last_run_admitted_old_files": last.as_ref().and_then(|summary| summary.pointer("/backfill/admitted_files").cloned()),
                "last_run_selected_old_files": last.as_ref().map(selected_total),
                "remaining_out_of_window": pointer("/backfill/remaining_out_of_window"),
                "watermark_mtime": pointer("/backfill/watermark_mtime"),
                "contiguous": pointer("/backfill/contiguous"),
                "discovery_truncated": last.as_ref().map(discovery_flag),
                "missing_sources": last.as_ref().map(missing_total),
                "note": "stored_visible_sources_by_client counts every source with stored visible text (regular window and historic); last_run_* describe only what the last collector run recorded. remaining/watermark are null when no run recorded them. Discovery gaps mean the covered prefix is lower than the file count suggests.",
            }));
        }
        Err(error) => {
            let status = if matches!(error, ArchiveError::NotConfigured) {
                "not_configured"
            } else {
                "error"
            };
            object.insert("status".into(), json!(status));
            object.insert("error_kind".into(), json!(error.kind()));
            object.insert("error".into(), json!(error.to_string()));
        }
    }
    document
}

/// The privacy panel of the settings window: what each archive client
/// collects, which roles, the age window, and how much is stored or
/// quarantined. Composed from the same status/config documents the CLI
/// prints; turning a role off only stops the collection — stored rows stay.
pub(crate) fn privacy_document(brain_home: &Path, home: &Path) -> Value {
    let status = status(brain_home, home);
    let config = &status["configured"]["collector_config"];
    let stored = &status["stored"];
    let quarantine_total: usize = stored["quarantined"]["by_reason"]
        .as_object()
        .map(|reasons| {
            reasons
                .values()
                .filter_map(|value| value.as_u64())
                .sum::<u64>() as usize
        })
        .unwrap_or(0);
    json!({
        "error": config["error"],
        "archive_status": status["status"],
        "enabled": config["enabled"].as_bool().unwrap_or(false),
        "clients_configured": config["clients_configured"],
        "client_selection": config["client_selection"],
        "roles_configured": config["roles_configured"],
        "role_selection": config["role_selection"],
        "since_days": config["since_days"],
        "stored_total": stored["entries"].as_u64().unwrap_or(0),
        "stored_by_client": stored["by_client"],
        "quarantine_total": quarantine_total,
        "quarantine_by_reason": stored["quarantined"]["by_reason"],
        "quarantine_by_client": stored["quarantined"]["by_client"],
        "note": "Выключение роли или клиента останавливает только сбор новых записей; уже сохранённое не удаляется.",
    })
}

/// Apply a settings-window privacy change through the same `set_enabled`
/// the CLI uses. Omitted fields keep their current configured values; every
/// validation (clients, roles, since_days range) is set_enabled's own, so an
/// invalid change is refused whole and nothing else moves.
pub(crate) fn apply_privacy(brain_home: &Path, payload: &Value) -> Result<Value, Box<dyn Error>> {
    let object = payload.as_object().ok_or("expected an object")?;
    if object.keys().any(|key| {
        !["confirm", "enabled", "since_days", "clients", "roles"].contains(&key.as_str())
    }) {
        return Err("unknown privacy field (allowed: enabled, since_days, clients, roles; roles are user, assistant, tool_call, tool_result, tool_error, reasoning)".into());
    }
    let current = read_config(&Paths::new(brain_home))?.unwrap_or_default();
    let enabled = match payload.get("enabled").or_else(|| current.get("enabled")) {
        Some(value) => value.as_bool().ok_or("enabled must be boolean")?,
        None => false,
    };
    let since_days = payload
        .get("since_days")
        .map(|value| {
            value
                .as_u64()
                .ok_or("since_days must be a positive integer")
        })
        .transpose()?;
    let clients = payload
        .get("clients")
        .map(validate_clients)
        .transpose()?
        .map(|items| items.into_iter().map(str::to_owned).collect::<Vec<_>>());
    let roles = payload
        .get("roles")
        .map(validate_roles)
        .transpose()?
        .map(|items| items.into_iter().map(str::to_owned).collect::<Vec<_>>());
    set_enabled(
        brain_home,
        enabled,
        since_days,
        clients.as_deref(),
        roles.as_deref(),
    )
}

fn selected_total(summary: &Value) -> i64 {
    summary
        .get("clients")
        .and_then(Value::as_object)
        .map(|clients| {
            clients
                .values()
                .map(|counts| counts.get("selected").and_then(Value::as_i64).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

fn last_run_summary(archive: &Archive) -> Option<Value> {
    archive
        .connection
        .query_row(
            "SELECT summary FROM collector_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
}

fn discovery_flag(summary: &Value) -> bool {
    summary
        .get("clients")
        .and_then(Value::as_object)
        .is_some_and(|clients| {
            clients.values().any(|counts| {
                counts
                    .get("discovery_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
        })
}

fn missing_total(summary: &Value) -> i64 {
    summary
        .get("clients")
        .and_then(Value::as_object)
        .map(|clients| {
            clients
                .values()
                .map(|counts| counts.get("missing").and_then(Value::as_i64).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

/// Writes owner-only bytes via a fresh temporary file and an atomic rename.
pub(crate) fn write_private(target: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if fs::symlink_metadata(target).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err("refusing to replace a symlink (remove the symlink yourself if you really want this path rewritten)".into());
    }
    let temporary = target.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, target)?;
    Ok(())
}

/// Runs the bundled collector once with fixed arguments and returns its counts.
pub(crate) fn collect(brain_home: &Path, home: &Path) -> Result<Value, Box<dyn Error>> {
    let paths = Paths::new(brain_home);
    ensure_private_dir(&paths.dir)?;
    let current = fs::read(&paths.helper).ok();
    if current.as_deref() != Some(COLLECTOR_SOURCE.as_bytes()) {
        write_private(&paths.helper, COLLECTOR_SOURCE.as_bytes())?;
    }
    let python = PYTHONS
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or("python3 not found at /usr/bin/python3 or /usr/local/bin/python3; the collector needs it")?;
    let mut child = Command::new(python)
        .arg("-I")
        .arg("-B")
        .arg(&paths.helper)
        .arg("--database")
        .arg(&paths.database)
        .arg("collect")
        .arg("--home")
        .arg(home)
        .arg("--config")
        .arg(&paths.config)
        .env_clear()
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes while the child runs, so a chatty collector cannot
    // block on a full pipe and be mistaken for a hung one.
    let drain = |pipe: Option<Box<dyn Read + Send>>, cap: u64| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(pipe) = pipe {
                let mut pipe = pipe;
                let _ = pipe.by_ref().take(cap).read_to_end(&mut text);
                let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout_reader = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_HELPER_OUTPUT,
    );
    let stderr_reader = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        4096,
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > COLLECT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("collector exceeded its time limit and was stopped".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let message: String = stderr
            .lines()
            .next()
            .unwrap_or("collector failed")
            .chars()
            .take(300)
            .collect();
        return Err(message.into());
    }
    Ok(serde_json::from_str(&stdout)?)
}

/// The user units that run `collect` every two minutes.
///
/// The binary path goes into `ExecStart=` verbatim, so anything systemd would
/// interpret there — quotes, escapes, `%` specifiers, `$` variables, control
/// characters — is refused rather than escaped.
pub(crate) fn timer_units(binary: &Path) -> Result<(String, String), Box<dyn Error>> {
    let text = binary.to_str().ok_or("binary path is not UTF-8")?;
    if !text.starts_with('/') {
        return Err("binary path must be absolute".into());
    }
    let binary_arg = systemd_argument(text)?;
    let service = format!(
        "[Unit]\nDescription=Brain for AI: bounded conversation archive collector\n\n\
         [Service]\nType=oneshot\nExecStart={} archive collect --quiet\nNice=10\n\
         IOSchedulingClass=idle\nTimeoutStartSec=180\n",
        binary_arg
    );
    let timer =
        "[Unit]\nDescription=Brain for AI: collect new conversation text every 2 minutes\n\n\
                 [Timer]\nOnBootSec=2min\nOnUnitInactiveSec=2min\nAccuracySec=30s\n\n\
                 [Install]\nWantedBy=timers.target\n"
            .to_owned();
    Ok((service, timer))
}

/// Quote one literal ExecStart argument, refusing systemd expansion and escapes.
pub(crate) fn systemd_argument(text: &str) -> Result<String, Box<dyn Error>> {
    if text.is_empty()
        || text
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\' | '\'' | '%' | '$' | ';'))
    {
        return Err(
            "argument cannot be written safely into a systemd unit; use a plain path or name"
                .into(),
        );
    }
    Ok(format!("\"{text}\""))
}

/// Writes (or removes) the unit files; enabling is left to systemctl.
pub(crate) fn write_timer_units(
    home: &Path,
    binary: &Path,
    install: bool,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let units = if install {
        Some(timer_units(binary)?)
    } else {
        None
    };
    write_named_timer_units(home, SERVICE_UNIT, TIMER_UNIT, units)
}

/// Shared private, atomic unit writer for archive and distillation timers.
pub(crate) fn write_named_timer_units(
    home: &Path,
    service_name: &str,
    timer_name: &str,
    units: Option<(String, String)>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let dir = home.join(".config/systemd/user");
    let service_path = dir.join(service_name);
    let timer_path = dir.join(timer_name);
    if let Some((service, timer)) = units {
        fs::create_dir_all(&dir)?;
        for (path, text) in [(&service_path, service), (&timer_path, timer)] {
            write_private(path, text.as_bytes())?;
        }
    } else {
        for path in [&timer_path, &service_path] {
            match fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(error.into());
                }
                _ => {}
            }
        }
    }
    Ok(vec![service_path, timer_path])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &[&str] = &[
        "CREATE TABLE archive_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        "CREATE TABLE sources(id INTEGER PRIMARY KEY, client TEXT NOT NULL, path TEXT NOT NULL, path_sha256 TEXT NOT NULL, UNIQUE(client, path))",
        "CREATE TABLE entries(id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL, session_id TEXT, role TEXT NOT NULL, locator TEXT NOT NULL, source_offset INTEGER, ts TEXT, content TEXT NOT NULL, content_sha256 TEXT NOT NULL, evidence_sha256 TEXT NOT NULL, record_sha256 TEXT NOT NULL, imported_at TEXT NOT NULL, cwd TEXT)",
        "CREATE VIRTUAL TABLE entries_fts USING fts5(content, content='entries', content_rowid='id', tokenize=\"unicode61 remove_diacritics 0 tokenchars '_'\")",
        "CREATE TABLE cursors(source_id INTEGER PRIMARY KEY, client TEXT NOT NULL, state TEXT NOT NULL, resets INTEGER NOT NULL DEFAULT 0, last_error TEXT, last_visit TEXT)",
        "CREATE TABLE collector_runs(id INTEGER PRIMARY KEY, started_at TEXT NOT NULL, finished_at TEXT NOT NULL, outcome TEXT NOT NULL, summary TEXT NOT NULL)",
    ];

    struct Fixture {
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn privacy_patch_rejects_invalid_types_and_preserves_untouched_settings() {
        let (_guard, home) = fixture("privacy-patch");
        set_enabled(
            &home,
            true,
            Some(21),
            Some(&["codex".to_owned()]),
            Some(&["user".to_owned()]),
        )
        .unwrap();
        let path = Paths::new(&home).config;
        let before = fs::read(&path).unwrap();
        for payload in [
            json!({"enabled":"false"}),
            json!({"enabled":null}),
            json!({"since_days":-1}),
            json!({"since_days":null}),
            json!({"since_days":1.5}),
            json!({"since_days":3651}),
            json!({"clients":"claude"}),
            json!({"clients":["claude",1]}),
            json!({"roles":["user",1]}),
            json!({"roles":null}),
            json!({"roles":[]}),
            json!({"roles":["user","user"]}),
            json!({"typo":true}),
        ] {
            assert!(apply_privacy(&home, &payload).is_err(), "{payload}");
            assert_eq!(
                fs::read(&path).unwrap(),
                before,
                "invalid patch writes nothing: {payload}"
            );
        }
        apply_privacy(&home, &json!({"roles":["assistant"]})).unwrap();
        let config = read_config(&Paths::new(&home)).unwrap().unwrap();
        assert_eq!(config["enabled"], true);
        assert_eq!(config["clients"], json!(["codex"]));
        assert_eq!(config["since_days"], 21);
        assert_eq!(config["roles"], json!(["assistant"]));
        fs::write(&path, b"broken JSON").unwrap();
        assert!(apply_privacy(&home, &json!({"enabled":false})).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"broken JSON");
    }

    fn fixture(_name: &str) -> (Fixture, PathBuf) {
        let root = crate::test_support::temp_path("brain-archive");
        let _ = fs::remove_dir_all(&root);
        let brain_home = root.join("brain");
        let dir = brain_home.join("archive");
        fs::create_dir_all(&dir).expect("dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        (Fixture { root }, brain_home)
    }

    fn build(brain_home: &Path, marker: &str, rows: &[(&str, &str, &str, &str, &str)]) -> PathBuf {
        let database = Paths::new(brain_home).database;
        let connection = Connection::open(&database).expect("open");
        for statement in SCHEMA {
            connection.execute(statement, []).expect("schema");
        }
        connection
            .execute("INSERT INTO archive_meta VALUES ('schema', ?1)", [marker])
            .expect("marker");
        for (index, client) in CLIENTS.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO sources VALUES (?1, ?2, ?3, 'pathsha')",
                    params![index as i64 + 1, client, format!("/src/{client}")],
                )
                .expect("source");
        }
        for (client, session, role, text, cwd) in rows {
            let source = CLIENTS
                .iter()
                .position(|name| name == client)
                .expect("client") as i64
                + 1;
            connection
                .execute(
                    "INSERT INTO entries(source_id, session_id, role, locator, ts, content, content_sha256,
                       evidence_sha256, record_sha256, imported_at, cwd)
                     VALUES (?1, ?2, ?3, 'uuid:x', '2026-10-08T10:00:00Z', ?4, 'c', 'e', 'r', '2026-10-08T10:00:05Z', ?5)",
                    params![source, session, role, text, if cwd.is_empty() { None } else { Some(*cwd) }],
                )
                .expect("entry");
            let id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO entries_fts(rowid, content) VALUES (?1, ?2)",
                    params![id, text],
                )
                .expect("fts");
        }
        drop(connection);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).expect("chmod");
        database
    }

    fn sample(brain_home: &Path) -> PathBuf {
        build(
            brain_home,
            SCHEMA_CURRENT,
            &[
                (
                    "claude",
                    "s-c",
                    "user",
                    "проверь parse_config_v2 сегодня",
                    "/work/brain",
                ),
                (
                    "claude",
                    "s-c",
                    "assistant",
                    "исправил parse_config_v2 готово",
                    "/work/brain",
                ),
                (
                    "codex",
                    "s-x",
                    "user",
                    "codex видит parse_config_v2",
                    "/work/brainstorm",
                ),
                (
                    "hermes",
                    "s-h",
                    "assistant",
                    "hermes NOT parse_config_v2 OR \"quoted\"",
                    "",
                ),
            ],
        )
    }

    fn query(text: &str) -> SearchRequest {
        SearchRequest {
            query: text.into(),
            ..SearchRequest::default()
        }
    }

    #[test]
    fn a_missing_archive_is_not_configured_and_nothing_is_created() {
        let (_guard, brain_home) = fixture("missing");
        let error = search(&brain_home, &query("x")).expect_err("missing");
        assert_eq!(error.kind(), "not_configured");
        let document = status(&brain_home, &brain_home);
        assert_eq!(document["status"], "not_configured");
        assert_eq!(document["complete_capture_claimed"], false);
        assert!(!Paths::new(&brain_home).database.exists());
    }

    #[test]
    fn search_reads_every_client_with_provenance_and_trust() {
        let (_guard, brain_home) = fixture("search");
        sample(&brain_home);
        let found = search(&brain_home, &query("parse_config_v2")).expect("search");
        assert_eq!(found["trust"], TRUST);
        let clients: Vec<&str> = found["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|hit| hit["client"].as_str().expect("client"))
            .collect();
        for client in CLIENTS {
            assert!(
                clients.contains(&client),
                "{client} missing from {clients:?}"
            );
        }
        let hit = &found["results"][0];
        assert!(hit["observed_at"].is_string() && hit["session_id"].is_string());
        let filtered = search(
            &brain_home,
            &SearchRequest {
                client: Some("codex".into()),
                ..query("parse_config_v2")
            },
        )
        .expect("filter");
        assert_eq!(filtered["results"].as_array().expect("results").len(), 1);
        let bad = search(
            &brain_home,
            &SearchRequest {
                client: Some("cursor".into()),
                ..query("x")
            },
        );
        assert_eq!(bad.expect_err("client").kind(), "bad_request");
    }

    #[test]
    fn hermes_profile_labels_come_from_the_path_shape() {
        assert_eq!(hermes_profile_label("/home/u/.hermes/state.db"), "default");
        assert_eq!(
            hermes_profile_label("/home/u/.hermes/profiles/sol/state.db"),
            "sol"
        );
        // Not the profiles/<name>/state.db shape, or a name the collector
        // would never have accepted: not a profile.
        assert_eq!(
            hermes_profile_label("/home/u/.hermes/sol/state.db"),
            "default"
        );
        assert_eq!(
            hermes_profile_label("/home/u/.hermes/profiles/sol/backup.db"),
            "default"
        );
        assert_eq!(hermes_profile_label("/x/profiles/../state.db"), "default");
    }

    #[test]
    fn status_counts_hermes_sources_per_profile() {
        let (_guard, brain_home) = fixture("hermes-profiles");
        let database = Paths::new(&brain_home).database;
        let connection = Connection::open(&database).expect("open");
        for statement in SCHEMA {
            connection.execute(statement, []).expect("schema");
        }
        connection
            .execute(
                "INSERT INTO archive_meta VALUES ('schema', 'brain_session_archive:3')",
                [],
            )
            .expect("marker");
        for (id, path) in [
            (1_i64, "/h/.hermes/state.db"),
            (2, "/h/.hermes/profiles/sol/state.db"),
        ] {
            connection
                .execute(
                    "INSERT INTO sources VALUES (?1, 'hermes', ?2, 'sha')",
                    params![id, path],
                )
                .expect("source");
        }
        for (source, session, text) in [
            (1_i64, "h9", "top-level message"),
            (2, "profiles/sol/h9", "profile message one"),
            (2, "profiles/sol/h9", "profile message two"),
        ] {
            connection
                .execute(
                    "INSERT INTO entries(source_id, session_id, role, locator, ts, content,
                       content_sha256, evidence_sha256, record_sha256, imported_at)
                     VALUES (?1, ?2, 'user', 'row:1', '1791400000.5', ?3, 'c', 'e', 'r',
                       '2026-10-08T10:00:05Z')",
                    params![source, session, text],
                )
                .expect("entry");
            let id = connection.last_insert_rowid();
            connection
                .execute(
                    "INSERT INTO entries_fts(rowid, content) VALUES (?1, ?2)",
                    params![id, text],
                )
                .expect("fts");
        }
        drop(connection);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).expect("chmod");
        let archive = open(&database).expect("archive");
        let stored = stored_document(&archive).expect("stored");
        assert_eq!(
            stored["hermes_profiles"],
            json!({
                "default": {"entries": 1, "sessions": 1, "sources": 1},
                "sol": {"entries": 2, "sessions": 1, "sources": 1},
            })
        );
        // The overall per-client counts keep their own shape.
        assert_eq!(stored["by_client"]["hermes"]["sources"], 2);
    }

    #[test]
    fn fts_operators_and_quotes_are_literal_text() {
        let (_guard, brain_home) = fixture("fts");
        sample(&brain_home);
        for text in [
            "NOT",
            "\"quoted\"",
            "parse_config_v2 OR",
            "content:hermes",
            "* ( ) ^ -",
            "NEAR(a b)",
        ] {
            let result = search(&brain_home, &query(text));
            assert!(result.is_ok(), "{text}: {result:?}");
        }
        let hits = search(&brain_home, &query("NOT")).expect("not");
        assert_eq!(hits["results"].as_array().expect("results").len(), 1);
        assert_eq!(fts_query("a\"b c").expect("query"), "\"a\"\"b\" \"c\"");
        assert_eq!(
            search(&brain_home, &query("   "))
                .expect_err("empty")
                .kind(),
            "bad_request"
        );
        let long = "x".repeat(MAX_QUERY_BYTES + 1);
        assert_eq!(
            search(&brain_home, &query(&long)).expect_err("long").kind(),
            "bad_request"
        );
    }

    #[test]
    fn match_any_bounds_dedupe_and_fold_yo() {
        // Sixteen content terms is the ceiling, and stop words interleaved
        // between them never consume a slot.
        let words = [
            "барабан",
            "чемодан",
            "картина",
            "корзина",
            "карандаш",
            "гитара",
            "колесо",
            "дерево",
            "звезда",
            "лодка",
            "мост",
            "поезд",
            "самолет",
            "телефон",
            "улица",
            "фонтан",
            "хозяйка",
            "церковь",
        ];
        let query = words
            .iter()
            .enumerate()
            .map(|(index, word)| {
                if index % 2 == 0 {
                    format!("the что {word}")
                } else {
                    (*word).to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let built = fts_query_any(&query).expect("query builds");
        let terms: Vec<&str> = built.split(" OR ").collect();
        assert_eq!(terms.len(), MAX_TERMS, "{built}");
        assert!(
            !built.contains("церковь"),
            "the 18th word is past the limit"
        );
        // Duplicates after lowercasing are one term, Ё/ё included.
        assert_eq!(
            fts_query_any("Отказ отказ ОТКАЗ")
                .unwrap()
                .split(" OR ")
                .count(),
            1
        );
        assert_eq!(
            fts_query_any("Ёлка ёлка ЁЛКА")
                .unwrap()
                .split(" OR ")
                .count(),
            1
        );
    }

    #[test]
    fn the_query_byte_bound_is_checked_before_terms() {
        // Cyrillic letters are two bytes each: 500 fit, 501 do not.
        assert!(fts_query_any(&"ф".repeat(500)).is_ok());
        let error = fts_query_any(&"ф".repeat(501)).expect_err("1002 bytes");
        assert!(error.to_string().contains("longer than"), "{error}");
    }

    #[test]
    fn match_any_keeps_content_words_digits_and_quotes_everything() {
        assert_eq!(
            fts_query_any("почему 9 октября отказались от Ollama Cloud?").expect("query"),
            "\"9\" OR \"октяб\"* OR \"отказ\"* OR \"ollama\" OR \"cloud\""
        );
        assert_eq!(
            fts_query_any("что было на порту 8803").expect("query"),
            "\"порту\" OR \"8803\""
        );
        // Operators and quotes stay text: each survives only inside a quoted term.
        assert_eq!(
            fts_query_any("NEAR(a b) \"x\" OR* col:val").expect("query"),
            "\"near(a\" OR \"col:v\"*"
        );
        // Stop words only: no lexical evidence and never an empty MATCH.
        assert_eq!(fts_query_any("что как где").expect("query"), "");
    }

    #[test]
    fn match_any_finds_a_spoken_question_strict_mode_does_not() {
        let (_guard, brain_home) = fixture("match-any");
        build(
            &brain_home,
            SCHEMA_CURRENT,
            &[(
                "codex",
                "c1",
                "user",
                "Ollama Cloud отказ: подписка просрочена",
                "",
            )],
        );
        let question = "почему отказались от Ollama Cloud";
        let strict = search(&brain_home, &query(question)).expect("strict");
        assert_eq!(strict["results"].as_array().expect("results").len(), 0);
        let any = search(
            &brain_home,
            &SearchRequest {
                match_any: true,
                ..query(question)
            },
        )
        .expect("any");
        assert_eq!(any["results"].as_array().expect("results").len(), 1);
        for hostile in [
            "\"",
            "*",
            "NEAR(a b)",
            "OR AND NOT",
            "-ollama",
            "col:val",
            "a\"b\" OR \"c",
        ] {
            let result = search(
                &brain_home,
                &SearchRequest {
                    match_any: true,
                    ..query(hostile)
                },
            );
            assert!(
                !matches!(result, Err(ArchiveError::Database(_))),
                "{hostile}: {result:?}"
            );
        }
    }

    #[test]
    fn match_any_stop_words_have_no_results_even_when_the_phrase_exists() {
        let (_guard, brain_home) = fixture("stop-only");
        build(
            &brain_home,
            SCHEMA_CURRENT,
            &[("codex", "c1", "user", "что как где the and for", "")],
        );
        for text in ["что как где", "the and for", "и на в", "a an the"] {
            let mut request = query(text);
            request.match_any = true;
            let found = search(&brain_home, &request).expect("valid empty search");
            assert_eq!(found["results"].as_array().unwrap().len(), 0);
            assert_eq!(found["tokens_used_estimate"], 0);
        }
        let strict = search(&brain_home, &query("что как где")).unwrap();
        assert_eq!(strict["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn project_root_matches_exact_directory_or_below_never_prefix_strings() {
        let (_guard, brain_home) = fixture("project");
        sample(&brain_home);
        let found = search(
            &brain_home,
            &SearchRequest {
                project_root: Some("/work/brain".into()),
                ..query("parse_config_v2")
            },
        )
        .expect("project");
        let results = found["results"].as_array().expect("results");
        assert_eq!(results.len(), 2, "{found}");
        assert!(results.iter().all(|hit| hit["cwd"] == "/work/brain"));
        let relative = search(
            &brain_home,
            &SearchRequest {
                project_root: Some("work".into()),
                ..query("x")
            },
        );
        assert_eq!(relative.expect_err("relative").kind(), "bad_request");
    }

    #[test]
    fn limits_and_token_budget_bound_the_answer() {
        let (_guard, brain_home) = fixture("budget");
        let long = format!("budget_word {}", "слово ".repeat(400));
        let rows: Vec<(&str, &str, &str, &str, &str)> = (0..30)
            .map(|_| ("claude", "s", "user", long.as_str(), ""))
            .collect();
        build(&brain_home, SCHEMA_CURRENT, &rows);
        let found = search(
            &brain_home,
            &SearchRequest {
                limit: Some(20),
                budget_tokens: Some(MIN_BUDGET),
                ..query("budget_word")
            },
        )
        .expect("search");
        assert!(found["tokens_used_estimate"].as_u64().expect("used") <= MIN_BUDGET as u64);
        assert_eq!(found["truncated_by_budget"], true);
        assert!(
            found["results"][0]["excerpt"]
                .as_str()
                .expect("excerpt")
                .chars()
                .count()
                <= EXCERPT_CHARS + 1
        );
        for (limit, budget) in [
            (Some(0), None),
            (Some(21), None),
            (None, Some(10)),
            (None, Some(5_000)),
        ] {
            let result = search(
                &brain_home,
                &SearchRequest {
                    limit,
                    budget_tokens: budget,
                    ..query("budget_word")
                },
            );
            assert_eq!(result.expect_err("bound").kind(), "bad_request");
        }
    }

    #[test]
    fn read_is_bounded_paged_and_carries_provenance() {
        let (_guard, brain_home) = fixture("read");
        let long = "ё".repeat(10_000);
        build(
            &brain_home,
            SCHEMA_CURRENT,
            &[
                ("claude", "s", "user", "before", ""),
                ("claude", "s", "assistant", long.as_str(), "/w"),
                ("claude", "s", "user", "after", ""),
            ],
        );
        let first = read(
            &brain_home,
            &ReadRequest {
                id: "2".into(),
                max_bytes: Some(1_001),
                context: Some(1),
                ..ReadRequest::default()
            },
        )
        .expect("read");
        assert_eq!(first["trust"], TRUST);
        assert_eq!(first["content_bytes"], 20_000);
        assert_eq!(first["content"].as_str().expect("content").len(), 1_000);
        assert_eq!(first["next_offset"], 1_000);
        assert_eq!(first["capture_lag_seconds"], 5);
        assert_eq!(first["neighbours"]["before"][0]["id"], 1);
        assert_eq!(first["neighbours"]["after"][0]["id"], 3);
        let next = read(
            &brain_home,
            &ReadRequest {
                id: "2".into(),
                offset: Some(19_999),
                ..ReadRequest::default()
            },
        )
        .expect("tail");
        assert_eq!(next["content_offset"], 19_998);
        assert!(next["next_offset"].is_null());
        for id in [
            "0",
            "-1",
            "abc",
            "99999999999999999999999",
            &"9".repeat(4_000),
        ] {
            let result = read(
                &brain_home,
                &ReadRequest {
                    id: id.into(),
                    ..ReadRequest::default()
                },
            );
            assert_eq!(result.expect_err("id").kind(), "bad_request", "{id}");
        }
        let missing = read(
            &brain_home,
            &ReadRequest {
                id: "999".into(),
                ..ReadRequest::default()
            },
        );
        assert_eq!(missing.expect_err("missing").kind(), "not_found");
        let bad = read(
            &brain_home,
            &ReadRequest {
                id: "1".into(),
                max_bytes: Some(1),
                ..ReadRequest::default()
            },
        );
        assert_eq!(bad.expect_err("small").kind(), "bad_request");
    }

    #[test]
    fn foreign_and_unknown_databases_are_refused_unchanged() {
        let (_guard, brain_home) = fixture("foreign");
        let database = Paths::new(&brain_home).database;
        let connection = Connection::open(&database).expect("open");
        connection
            .execute("CREATE TABLE nodes(id TEXT)", [])
            .expect("table");
        drop(connection);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).expect("chmod");
        let before = fs::read(&database).expect("bytes");
        assert_eq!(
            search(&brain_home, &query("x"))
                .expect_err("foreign")
                .kind(),
            "incompatible_archive"
        );
        assert_eq!(fs::read(&database).expect("bytes"), before);
        fs::write(
            &database,
            b"not sqlite at all, just text that is long enough",
        )
        .expect("write");
        assert_eq!(
            search(&brain_home, &query("x")).expect_err("junk").kind(),
            "incompatible_archive"
        );
        fs::remove_file(&database).expect("rm");
        build(&brain_home, "brain_session_archive:9", &[]);
        assert_eq!(
            search(&brain_home, &query("x"))
                .expect_err("version")
                .kind(),
            "incompatible_archive"
        );
    }

    #[test]
    fn permissive_or_symlinked_archives_are_refused() {
        let (_guard, brain_home) = fixture("perm");
        let database = sample(&brain_home);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            search(&brain_home, &query("x")).expect_err("mode").kind(),
            "unsafe_permissions"
        );
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).expect("chmod");
        let real = database.with_extension("real");
        fs::rename(&database, &real).expect("mv");
        std::os::unix::fs::symlink(&real, &database).expect("symlink");
        assert_eq!(
            search(&brain_home, &query("x")).expect_err("link").kind(),
            "unsafe_permissions"
        );
    }

    #[test]
    fn a_schema_two_archive_is_read_without_project_filters() {
        let (_guard, brain_home) = fixture("legacy");
        build(
            &brain_home,
            SCHEMA_LEGACY,
            &[("hermes", "h", "user", "старый архив", "")],
        );
        let found = search(&brain_home, &query("архив")).expect("legacy");
        assert_eq!(found["results"][0]["cwd"], Value::Null);
        let project = search(
            &brain_home,
            &SearchRequest {
                project_root: Some("/w".into()),
                ..query("архив")
            },
        );
        assert_eq!(project.expect_err("schema2").kind(), "bad_request");
        assert_eq!(status(&brain_home, &brain_home)["status"], "ok");
    }

    #[test]
    fn quarantined_entries_are_kept_but_never_searched_read_or_counted() {
        let (_fixture, brain_home) = fixture("quarantine");
        let database = build(
            &brain_home,
            SCHEMA_CURRENT,
            &[
                (
                    "codex",
                    "x",
                    "assistant",
                    "[external_agent_tool_result]\nwrapper_payload shared_word",
                    "",
                ),
                (
                    "codex",
                    "x",
                    "assistant",
                    "ordinary_visible shared_word",
                    "",
                ),
                (
                    "codex",
                    "x",
                    "user",
                    "I quote [external_agent_tool_result] shared_word",
                    "",
                ),
            ],
        );
        let connection = Connection::open(&database).expect("open");
        connection
            .execute_batch(
                "CREATE TABLE quarantine(entry_id INTEGER PRIMARY KEY, reason TEXT NOT NULL, rule TEXT NOT NULL,
                   evidence TEXT NOT NULL, quarantined_at TEXT NOT NULL, released_at TEXT);
                 INSERT INTO quarantine VALUES (1, 'external_agent_tool_traffic', 'r', '{}', 't', NULL);",
            )
            .expect("quarantine");
        let privacy = privacy_document(&brain_home, &brain_home);
        assert_eq!(privacy["stored_by_client"]["codex"]["entries"], 2);
        assert_eq!(privacy["quarantine_by_client"]["codex"], 1);
        assert_eq!(privacy["quarantine_total"], 1);
        let ids = |text: &str| -> Vec<i64> {
            search(&brain_home, &query(text)).expect("search")["results"]
                .as_array()
                .expect("results")
                .iter()
                .map(|result| result["id"].as_i64().expect("id"))
                .collect()
        };
        assert_eq!(ids("shared_word"), vec![2, 3]);
        assert!(ids("wrapper_payload").is_empty());
        let hidden = read(
            &brain_home,
            &ReadRequest {
                id: "1".into(),
                ..Default::default()
            },
        );
        assert_eq!(hidden.expect_err("quarantined").kind(), "not_found");
        let near = read(
            &brain_home,
            &ReadRequest {
                id: "2".into(),
                context: Some(2),
                ..Default::default()
            },
        )
        .expect("read");
        assert!(
            near["neighbours"]["before"]
                .as_array()
                .expect("before")
                .is_empty()
        );
        let stored = stored_document(&open(&database).expect("archive")).expect("stored");
        assert_eq!(stored["entries"], 2);
        assert_eq!(
            stored["quarantined"]["by_reason"]["external_agent_tool_traffic"],
            1
        );
        let kept: i64 = connection
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))
            .expect("count");
        assert_eq!(kept, 3, "quarantine never deletes the stored entry");
        connection
            .execute("UPDATE quarantine SET released_at = 'later'", [])
            .expect("release");
        assert_eq!(ids("wrapper_payload"), vec![1]);
    }

    #[test]
    fn status_separates_configured_observed_and_stored() {
        let (_guard, brain_home) = fixture("status");
        let database = sample(&brain_home);
        let connection = Connection::open(&database).expect("open");
        connection.execute("INSERT INTO cursors VALUES (1, 'claude', 'caught_up', 0, NULL, 't'), (2, 'codex', 'error', 1, 'PermissionError', 't')", []).expect("cursors");
        connection.execute("INSERT INTO collector_runs(started_at, finished_at, outcome, summary) VALUES ('a', 'b', 'ok', '{\"stopped_by\": null, \"clients\": {\"claude\": {\"discovered\": 3, \"out_of_window\": 2, \"secret\": \"x\"}}}')", []).expect("run");
        drop(connection);
        let document = status(&brain_home, &brain_home);
        assert_eq!(document["stored"]["by_client"]["claude"]["entries"], 2);
        assert_eq!(
            document["observed"]["cursors"]["codex"]["sources_by_state"]["error"],
            1
        );
        assert_eq!(
            document["observed"]["errors"][0]["error"],
            "PermissionError"
        );
        assert_eq!(
            document["observed"]["last_run"]["clients"]["claude"]["out_of_window"],
            2
        );
        assert!(
            document["observed"]["last_run"]["clients"]["claude"]
                .get("secret")
                .is_none()
        );
        assert_eq!(document["configured"]["collector_config"]["enabled"], false);
        assert_eq!(
            document["configured"]["timer"]["unit_files_installed"],
            false
        );
    }

    #[test]
    fn enable_and_disable_persist_a_private_validated_switch() {
        let (_guard, brain_home) = fixture("switch");
        let paths = Paths::new(&brain_home);
        set_enabled(&brain_home, true, Some(7), None, None).expect("enable");
        let config = read_config(&paths).expect("read").expect("present");
        assert_eq!(
            (config["enabled"].clone(), config["since_days"].clone()),
            (json!(true), json!(7))
        );
        assert_eq!(
            fs::metadata(&paths.config).expect("meta").mode() & 0o777,
            0o600
        );
        set_enabled(&brain_home, false, None, None, None).expect("disable");
        let config = read_config(&paths).expect("read").expect("present");
        assert_eq!(
            (config["enabled"].clone(), config["since_days"].clone()),
            (json!(false), json!(7))
        );
        assert!(set_enabled(&brain_home, true, Some(0), None, None).is_err());
        assert_eq!(
            read_config(&paths).expect("read").expect("present")["enabled"],
            json!(false)
        );
    }

    #[test]
    fn client_selection_is_validated_and_preserved_with_budgets() {
        let (_guard, brain_home) = fixture("clients");
        let paths = Paths::new(&brain_home);
        set_enabled(&brain_home, true, Some(3), None, None).expect("enable");
        let document = config_document(&paths);
        assert_eq!(document["client_selection"], json!("legacy_all"));
        assert_eq!(
            document["clients_configured"],
            json!(["claude", "codex", "hermes"])
        );
        // a budget written by hand survives every switch change
        let mut config = read_config(&paths).expect("read").expect("present");
        config.insert("max_seconds".into(), json!(9));
        let text = serde_json::to_string(&config).expect("json");
        write_private(&paths.config, text.as_bytes()).expect("write");
        let pick = vec!["hermes".to_string(), "codex".to_string()];
        set_enabled(&brain_home, true, None, Some(&pick), None).expect("select");
        set_enabled(&brain_home, false, None, None, None).expect("disable");
        set_enabled(&brain_home, true, None, None, None).expect("re-enable");
        let config = read_config(&paths).expect("read").expect("present");
        assert_eq!(config["clients"], json!(["codex", "hermes"]));
        assert_eq!(config["max_seconds"], json!(9));
        assert_eq!(config["since_days"], json!(3));
        assert_eq!(
            config_document(&paths)["client_selection"],
            json!("explicit")
        );
        let before = fs::read(&paths.config).expect("bytes");
        let duplicate = vec!["codex".to_string(), "codex".to_string()];
        for bad in [vec![], vec!["cursor".to_string()], duplicate] {
            assert!(
                set_enabled(&brain_home, true, None, Some(&bad), None).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(fs::read(&paths.config).expect("bytes"), before);
        let foreign = br#"{"version":1,"enabled":true,"clients":["cursor"]}"#;
        write_private(&paths.config, foreign).expect("write");
        let document = config_document(&paths);
        assert_eq!(document["enabled"], json!(false));
        assert!(
            document["error"]
                .as_str()
                .expect("error")
                .contains("clients")
        );
    }

    #[test]
    fn timer_units_run_the_stable_binary_without_a_shell() {
        let (service, timer) = timer_units(Path::new("/home/u/.brain/bin/brain")).expect("units");
        assert!(service.contains("ExecStart=\"/home/u/.brain/bin/brain\" archive collect --quiet"));
        assert!(service.contains("Type=oneshot"));
        assert!(timer.contains("OnUnitInactiveSec=2min"));
        assert!(!service.contains("sh -c"));
        for hostile in [
            "/x\"\nExecStartPre=/bin/evil",
            "/x%h",
            "/x$HOME",
            "/x\\\\",
            "relative/brain",
            "/x;y",
        ] {
            assert!(timer_units(Path::new(hostile)).is_err(), "{hostile}");
        }
    }

    // ---------------------------------------------------------------- backfill

    #[test]
    fn backfill_missing_is_disabled_and_creates_nothing() {
        let (_guard, brain_home) = fixture("bf-missing");
        let paths = Paths::new(&brain_home);
        let status = backfill_status(&brain_home);
        assert_eq!(status["state"], "disabled");
        assert_eq!(status["present"], false);
        assert!(
            !paths.backfill.exists(),
            "status must not create backfill.json"
        );
        let document = backfill_document(&brain_home);
        assert_eq!(document["complete_capture_claimed"], false);
        assert_eq!(document["backfill"]["state"], "disabled");
    }

    #[test]
    fn backfill_start_and_stop_write_a_private_atomic_switch() {
        let (_guard, brain_home) = fixture("bf-switch");
        let paths = Paths::new(&brain_home);
        let started = backfill_start(&brain_home, 7).expect("start");
        assert_eq!(
            (started["state"].clone(), started["files_per_run"].clone()),
            (json!("running"), json!(7))
        );
        assert_eq!(
            fs::metadata(&paths.backfill).expect("meta").mode() & 0o777,
            0o600
        );
        assert!(read_backfill(&paths).expect("read").is_some());
        // Restart with a new quota replaces the value.
        backfill_start(&brain_home, 3).expect("restart");
        assert_eq!(backfill_status(&brain_home)["files_per_run"], 3);
        // Stop keeps the quota but flips state; a later start resumes.
        let stopped = backfill_stop(&brain_home).expect("stop");
        assert_eq!(
            (stopped["state"].clone(), stopped["files_per_run"].clone()),
            (json!("stopped"), json!(3))
        );
        assert_eq!(backfill_status(&brain_home)["state"], "stopped");
    }

    #[test]
    fn backfill_quota_is_bounded_one_to_twenty() {
        let (_guard, brain_home) = fixture("bf-quota");
        assert!(
            backfill_start(&brain_home, 0).is_err(),
            "0 is not a valid start quota"
        );
        assert!(backfill_start(&brain_home, 21).is_err(), "above the cap");
        assert!(backfill_start(&brain_home, 1).is_ok());
        assert!(backfill_start(&brain_home, 20).is_ok());
        let paths = Paths::new(&brain_home);
        // A hand-written 21 in the file is refused, not clamped.
        fs::write(
            &paths.backfill,
            br#"{"v":1,"files_per_run":21,"state":"running"}"#,
        )
        .expect("write");
        fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(read_backfill(&paths).is_err());
        assert_eq!(backfill_status(&brain_home)["state"], "error");
    }

    #[test]
    fn backfill_config_is_strict_about_keys_types_and_version() {
        let (_guard, brain_home) = fixture("bf-strict");
        let paths = Paths::new(&brain_home);
        let write = |body: &str| {
            fs::write(&paths.backfill, body).expect("write");
            fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o600)).expect("chmod");
        };
        for bad in [
            r#"{"v":true,"files_per_run":2,"state":"running"}"#,
            r#"{"v":"1","files_per_run":2,"state":"running"}"#,
            r#"{"v":1,"files_per_run":"2","state":"running"}"#,
            r#"{"v":1,"files_per_run":2,"state":"paused"}"#,
            r#"{"v":1,"files_per_run":2,"state":"running","extra":1}"#,
            r#"{"v":1,"files_per_run":-1,"state":"running"}"#,
            "not json",
            "[1,2,3]",
        ] {
            write(bad);
            assert!(read_backfill(&paths).is_err(), "{bad}");
        }
        // A hand-written file is left exactly as it was, never auto-repaired.
        write("not json");
        let before = fs::read(&paths.backfill).expect("before");
        assert!(read_backfill(&paths).is_err());
        assert_eq!(fs::read(&paths.backfill).expect("after"), before);
    }

    #[test]
    fn backfill_config_symlink_and_foreign_mode_fail_closed() {
        let (_guard, brain_home) = fixture("bf-unsafe");
        let paths = Paths::new(&brain_home);
        // Group-writable mode is refused.
        write_backfill(&brain_home, 2, "running").expect("write");
        fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(read_backfill(&paths).is_err());
        assert_eq!(backfill_status(&brain_home)["state"], "error");
        // A symlink is refused rather than followed.
        let target = brain_home.join("elsewhere.json");
        fs::write(&target, br#"{"v":1,"files_per_run":2,"state":"running"}"#).expect("target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod");
        fs::remove_file(&paths.backfill).expect("remove");
        std::os::unix::fs::symlink(&target, &paths.backfill).expect("symlink");
        assert!(read_backfill(&paths).is_err());
    }

    #[test]
    fn backfill_never_touches_the_collector_config() {
        let (_guard, brain_home) = fixture("bf-isolation");
        let paths = Paths::new(&brain_home);
        set_enabled(&brain_home, true, Some(9), None, None).expect("enable collector");
        let collector_before = fs::read(&paths.config).expect("config");
        backfill_start(&brain_home, 2).expect("start backfill");
        backfill_stop(&brain_home).expect("stop backfill");
        assert_eq!(fs::read(&paths.config).expect("config"), collector_before);
        // Stopping backfill must not pause the regular collector.
        let config = read_config(&paths).expect("read").expect("present");
        assert_eq!(config["enabled"], json!(true));
    }

    #[test]
    fn backfill_document_is_honest_with_and_without_a_run() {
        let (_guard, brain_home) = fixture("bf-document");
        let database = sample(&brain_home);
        // No run recorded: remaining/watermark stay null, stored counts only visible text.
        let document = backfill_document(&brain_home);
        assert_eq!(document["status"], "ok");
        assert_eq!(document["coverage"]["remaining_out_of_window"], Value::Null);
        assert_eq!(document["coverage"]["watermark_mtime"], Value::Null);
        assert_eq!(
            document["coverage"]["last_run_admitted_old_files"],
            Value::Null
        );
        assert_eq!(
            document["coverage"]["stored_visible_sources_by_client"]["claude"],
            1
        );
        assert_eq!(document["complete_capture_claimed"], false);
        // With a run summary, the honest fields are surfaced, never invented.
        let connection = Connection::open(&database).expect("open");
        connection
            .execute(
                "INSERT INTO collector_runs(started_at, finished_at, outcome, summary) VALUES ('a','b','ok', ?1)",
                ["{\"backfill\":{\"admitted_files\":2,\"remaining_out_of_window\":4,\"watermark_mtime\":null,\"contiguous\":false},\"clients\":{\"claude\":{\"selected\":3,\"missing\":2,\"discovery_truncated\":true}}}"],
            )
            .expect("run");
        drop(connection);
        let document = backfill_document(&brain_home);
        assert_eq!(document["coverage"]["remaining_out_of_window"], 4);
        assert_eq!(document["coverage"]["watermark_mtime"], Value::Null);
        assert_eq!(document["coverage"]["contiguous"], false);
        assert_eq!(document["coverage"]["last_run_admitted_old_files"], 2);
        assert_eq!(document["coverage"]["last_run_selected_old_files"], 3);
        assert_eq!(document["coverage"]["missing_sources"], 2);
        assert_eq!(document["coverage"]["discovery_truncated"], true);
    }

    #[test]
    fn stored_visible_sources_counts_regular_and_backfill_but_not_quarantined_only() {
        // Defect 1 regression: the displayed "stored" count must not conflate a
        // regular-window source with a historic one, and a source whose only entry
        // is quarantined must not be counted at all.
        let (_guard, brain_home) = fixture("bf-counts");
        let database = Paths::new(&brain_home).database;
        let connection = Connection::open(&database).expect("open");
        for statement in SCHEMA {
            connection.execute(statement, []).expect("schema");
        }
        connection
            .execute(
                "INSERT INTO archive_meta VALUES ('schema', ?1)",
                [SCHEMA_CURRENT],
            )
            .expect("marker");
        // Three claude sources: a regular-window one, a historic one, and one whose
        // only entry is quarantined.
        for (id, path) in [
            (1, "/src/regular"),
            (2, "/src/old-backfill"),
            (3, "/src/quarantined-only"),
        ] {
            connection
                .execute(
                    "INSERT INTO sources VALUES (?1, 'claude', ?2, 'sha')",
                    params![id, path],
                )
                .expect("source");
        }
        for id in 1..=3 {
            connection
                .execute(
                    "INSERT INTO entries(id, source_id, session_id, role, locator, ts, content, content_sha256,
                       evidence_sha256, record_sha256, imported_at) VALUES (?1, ?1, 's', 'user', 'uuid:x',
                       '2026-10-08T10:00:00Z', ?2, 'c', ?3, 'r', '2026-10-08T10:00:05Z')",
                    params![id, format!("word_{id}"), format!("e{id}")],
                )
                .expect("entry");
        }
        connection
            .execute_batch(
                "CREATE TABLE quarantine(entry_id INTEGER PRIMARY KEY, reason TEXT NOT NULL, rule TEXT NOT NULL,
                   evidence TEXT NOT NULL, quarantined_at TEXT NOT NULL, released_at TEXT);
                 INSERT INTO quarantine VALUES (3, 'external_agent_tool_traffic', 'r', '{}', 't', NULL);",
            )
            .expect("quarantine");
        drop(connection);
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).expect("chmod");
        let document = backfill_document(&brain_home);
        let counts = &document["coverage"]["stored_visible_sources_by_client"];
        // Two visible sources for claude (the quarantined-only one is excluded).
        assert_eq!(
            counts["claude"], 2,
            "counts must exclude quarantined-only sources"
        );
        // The name is accurate: this is not labeled as a historic-import count.
        assert!(
            document["coverage"]
                .get("admitted_sources_by_client")
                .is_none()
        );
    }

    #[test]
    fn backfill_config_requires_every_field_matching_the_collector() {
        // Defect 4 regression: missing v / files_per_run / state are all refused.
        let (_guard, brain_home) = fixture("bf-required");
        let paths = Paths::new(&brain_home);
        for bad in [
            r#"{"files_per_run":2,"state":"running"}"#,
            r#"{"v":1,"state":"running"}"#,
            r#"{"v":1,"files_per_run":2}"#,
            r#"{}"#,
        ] {
            fs::write(&paths.backfill, bad).expect("write");
            fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o600)).expect("chmod");
            assert!(read_backfill(&paths).is_err(), "{bad}");
        }
    }

    #[test]
    fn backfill_start_refuses_a_malformed_existing_config_and_preserves_its_bytes() {
        // Defect 4 regression: start must not silently repair/replace a broken file.
        let (_guard, brain_home) = fixture("bf-no-repair");
        let paths = Paths::new(&brain_home);
        let broken = b"{\"v\":1,\"files_per_run\":99,\"state\":\"running\"}\n";
        fs::write(&paths.backfill, broken).expect("write");
        fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(
            backfill_start(&brain_home, 2).is_err(),
            "start must refuse a malformed config"
        );
        assert_eq!(
            fs::read(&paths.backfill).expect("read"),
            broken,
            "bytes preserved"
        );
        // A missing file is fine: start creates a fresh valid one.
        fs::remove_file(&paths.backfill).expect("remove");
        backfill_start(&brain_home, 2).expect("create");
        assert!(read_backfill(&paths).expect("read").is_some());
    }

    #[test]
    fn backfill_config_rejects_a_fifo_without_blocking() {
        // Defect 3 regression: a FIFO must be refused, and the call must not hang.
        let (_guard, brain_home) = fixture("bf-fifo");
        let paths = Paths::new(&brain_home);
        let status = std::process::Command::new("mkfifo")
            .arg(&paths.backfill)
            .status()
            .expect("mkfifo");
        assert!(status.success(), "mkfifo must succeed");
        // If read_backfill blocked on open(2), this test would time out.
        let result = read_backfill(&paths);
        assert!(matches!(result, Err(ArchiveError::Unsafe(_))), "{result:?}");
        assert_eq!(backfill_status(&brain_home)["state"], "error");
    }

    #[test]
    fn backfill_config_rejects_an_oversized_file_not_a_valid_prefix() {
        // Defect 3 regression: a valid prefix + > 64 KiB tail must be refused.
        let (_guard, brain_home) = fixture("bf-oversize");
        write_backfill(&brain_home, 2, "running").expect("write");
        let paths = Paths::new(&brain_home);
        let mut bytes = fs::read(&paths.backfill).expect("read");
        bytes.resize(64 * 1024 + 4096, b' ');
        fs::write(&paths.backfill, &bytes).expect("grow");
        fs::set_permissions(&paths.backfill, fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(read_backfill(&paths).is_err());
    }
}
