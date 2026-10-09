//! `map.sqlite`: the append-only ledger and its rebuildable `links` projection.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};

use super::events::{
    Authority, BASES, Cardinality, EVENT_VERSION, Event, EventDraft, MAX_SEQ, RELATION_POLICY,
    check_authority, parse_event_id, relation_policy, validate_draft, validate_machine,
};
use super::{MapError, MapErrorKind};

/// "BRNM": marks a file as this map's.
pub const APPLICATION_ID: i64 = 0x4252_4E4D;
pub const SCHEMA_VERSION: i64 = 1;
pub const FORMAT: &str = "brain-map/1";
/// Page limit for every listing.
pub const MAX_PAGE: usize = 100;
/// Events one projection may read into memory before refusing.
pub const MAX_PROJECTION_EVENTS: i64 = 100_000;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT;
CREATE TABLE events (
    local_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    v INTEGER NOT NULL,
    origin_machine TEXT NOT NULL,
    origin_seq INTEGER NOT NULL,
    authority TEXT NOT NULL,
    kind TEXT NOT NULL,
    space TEXT NOT NULL,
    subject TEXT NOT NULL,
    relation TEXT NOT NULL,
    object TEXT NOT NULL,
    basis TEXT NOT NULL,
    asserted_kind TEXT NOT NULL,
    client TEXT NOT NULL,
    agent TEXT NOT NULL,
    evidence TEXT NOT NULL,
    occurred_at INTEGER,
    occurred_src TEXT NOT NULL,
    recorded_at INTEGER NOT NULL,
    received_at INTEGER NOT NULL,
    confidence_milli INTEGER NOT NULL,
    corrects TEXT,
    payload TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    UNIQUE (origin_machine, origin_seq)
) STRICT;
CREATE INDEX events_slot ON events(space, subject, relation);
CREATE INDEX events_corrects ON events(corrects);
CREATE INDEX events_space_seq ON events(space, local_seq);
CREATE INDEX events_subject_seq ON events(space, subject, local_seq);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'brain-map events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'brain-map events are append-only'); END;
CREATE TABLE links (
    space TEXT NOT NULL,
    subject TEXT NOT NULL,
    relation TEXT NOT NULL,
    object TEXT NOT NULL,
    status TEXT NOT NULL,
    contested INTEGER NOT NULL,
    resolution TEXT NOT NULL,
    actors TEXT NOT NULL,
    current_events TEXT NOT NULL,
    all_events TEXT NOT NULL,
    best_basis TEXT NOT NULL,
    confidence_milli INTEGER NOT NULL,
    PRIMARY KEY (space, subject, relation, object)
) STRICT, WITHOUT ROWID;
CREATE TABLE ingest_cursors (
    source_kind TEXT NOT NULL,
    identity TEXT NOT NULL,
    space TEXT NOT NULL,
    path TEXT NOT NULL,
    position INTEGER NOT NULL,
    last_hash TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (source_kind, identity, space)
) STRICT;
CREATE TABLE archive_installs (
    install TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    bound_at INTEGER NOT NULL
) STRICT;
CREATE TABLE ingest_seen (
    source_kind TEXT NOT NULL,
    space TEXT NOT NULL,
    key TEXT NOT NULL,
    event_id TEXT NOT NULL,
    PRIMARY KEY (source_kind, space, key)
) STRICT, WITHOUT ROWID;
";
const TABLES: [&str; 6] = [
    "meta",
    "events",
    "links",
    "ingest_cursors",
    "archive_installs",
    "ingest_seen",
];
pub(super) const COLUMNS: &str = "v, origin_machine, origin_seq, authority, recorded_at, kind, space, subject, relation, object, \
    basis, asserted_kind, client, agent, evidence, occurred_at, occurred_src, confidence_milli, corrects, payload, content_hash";

/// One projected row: one object of one `(space, subject, relation)` slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkRow {
    pub space: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    /// proposed | confirmed | rejected | conflict | overridden | superseded
    pub status: String,
    /// Single-valued: two actors ever asserted different objects, or one slot
    /// holds several live values. Multi-valued: a local user both confirmed and
    /// rejected this very object. Only a local user resolves it.
    pub contested: bool,
    /// `user_confirmed` (a local user decided this row's slot or object) or `none`.
    pub resolution: String,
    pub actors: Vec<String>,
    pub current_events: Vec<String>,
    pub all_events: Vec<String>,
    pub best_basis: String,
    pub confidence_milli: i64,
}

/// What `replay` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Replayed {
    Inserted,
    Duplicate,
}

type Clock = Box<dyn Fn() -> i64 + Send>;

fn system_clock() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What a read-only look at the map found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MapStatus {
    /// No map file yet; nothing was created. A context can fall back to the coordinator, labelled so.
    NotConfigured,
    Ready {
        machine: String,
        events: i64,
        links: i64,
        cursors: i64,
    },
}

/// Read-only status of the map at `path`.
///
/// # Errors
///
/// As [`MapStore::open_read_only`], except that a missing map is `NotConfigured`.
pub fn map_status(path: &Path) -> Result<MapStatus, MapError> {
    let store = match MapStore::open_read_only(path) {
        Ok(store) => store,
        Err(e) if e.kind == MapErrorKind::NotFound => return Ok(MapStatus::NotConfigured),
        Err(e) => return Err(e),
    };
    let count = |table: &str| -> Result<i64, MapError> {
        Ok(store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    };
    Ok(MapStatus::Ready {
        machine: store.machine.clone(),
        events: count("events")?,
        links: count("links")?,
        cursors: count("ingest_cursors")?,
    })
}

/// Where the map lives under a Brain home.
#[must_use]
pub fn database_path(brain_home: &Path) -> PathBuf {
    brain_home.join("map").join("map.sqlite")
}

/// The map over one private database file.
pub struct MapStore {
    connection: Connection,
    machine: String,
    clock: Clock,
}

impl std::fmt::Debug for MapStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapStore")
            .field("machine", &self.machine)
            .finish_non_exhaustive()
    }
}

fn current_uid() -> Result<u32, MapError> {
    Ok(fs::metadata("/proc/self")?.uid())
}

/// Refuses a file or directory not owned by us or open to group/other.
fn check_private(path: &Path, mode: u32) -> Result<(), MapError> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(MapError::new(
            MapErrorKind::Incompatible,
            format!("{} is a symlink", path.display()),
        ));
    }
    if meta.uid() != current_uid()? || meta.mode() & 0o777 != mode {
        return Err(MapError::new(
            MapErrorKind::Incompatible,
            format!(
                "{} must be owned by this user with mode {mode:o}",
                path.display()
            ),
        ));
    }
    Ok(())
}

impl MapStore {
    /// Opens for writing, creating a private `brain-map/1` file if absent.
    ///
    /// # Errors
    ///
    /// `incompatible` for a foreign, newer, other-machine or non-private file.
    pub fn open(path: &Path, machine: &str) -> Result<Self, MapError> {
        validate_machine(machine)?;
        let dir = path
            .parent()
            .ok_or_else(|| MapError::invalid("map path has no directory"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        check_private(dir, 0o700)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        check_private(path, 0o600)?;
        // NOFOLLOW: a symlink swapped in after the check above is still refused.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        let mut store = Self {
            connection,
            machine: machine.to_owned(),
            clock: Box::new(system_clock),
        };
        store.initialise()?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if side.exists() {
                fs::set_permissions(&side, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            }
        }
        Ok(store)
    }

    /// Opens an existing map for reading only. Creates nothing (no directory,
    /// file or schema), migrates nothing, rebuilds nothing and changes no
    /// mode; refuses what `open` would refuse. SQLite may still create or use
    /// the `-wal`/`-shm` side files of a WAL database for a reader; those must
    /// already be private if present. Every write through this store fails.
    ///
    /// # Errors
    ///
    /// `not_found` when the map is not configured; `incompatible` for a
    /// foreign, newer, symlinked or non-private file.
    pub fn open_read_only(path: &Path) -> Result<Self, MapError> {
        let dir = path
            .parent()
            .ok_or_else(|| MapError::invalid("map path has no directory"))?;
        let missing = |p: &Path| matches!(fs::symlink_metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        if missing(dir) || missing(path) {
            return Err(MapError::new(
                MapErrorKind::NotFound,
                "the work map is not configured",
            ));
        }
        check_private(dir, 0o700)?;
        check_private(path, 0o600)?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            if !missing(&side) {
                check_private(&side, 0o600)?;
            }
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let connection = Connection::open_with_flags(path, flags)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "query_only", true)?;
        let machine: String = connection
            .query_row("SELECT value FROM meta WHERE key = 'machine_id'", [], |r| {
                r.get(0)
            })
            .map_err(|_| MapError::new(MapErrorKind::Incompatible, "not a Brain map database"))?;
        validate_machine(&machine).map_err(|_| {
            MapError::new(MapErrorKind::Incompatible, "map machine id is malformed")
        })?;
        let store = Self {
            connection,
            machine,
            clock: Box::new(system_clock),
        };
        store.check_schema()?;
        Ok(store)
    }

    /// Replaces the clock. Tests only.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    #[must_use]
    pub fn machine(&self) -> &str {
        &self.machine
    }

    fn initialise(&mut self) -> Result<(), MapError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let app: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let objects: i64 = tx.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
        if app == 0 && version == 0 && objects == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES ('format', ?1), ('machine_id', ?2), ('relations', ?3), \
                 ('ingest', ?4), ('next_seq', '1'), ('clock', '0')",
                params![FORMAT, self.machine, RELATION_POLICY, super::ingest::INGEST_FORMAT],
            )?;
            tx.pragma_update(None, "application_id", APPLICATION_ID)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            return Ok(());
        }
        drop(tx);
        self.check_schema()
    }

    fn check_schema(&self) -> Result<(), MapError> {
        let c = &self.connection;
        let incompatible = |m: String| MapError::new(MapErrorKind::Incompatible, m);
        let app: i64 = c.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if app != APPLICATION_ID {
            return Err(incompatible(format!(
                "not a Brain map database (application_id {app:#x})"
            )));
        }
        if version != SCHEMA_VERSION {
            return Err(incompatible(format!(
                "map schema {version} is not the supported {SCHEMA_VERSION}; refusing to guess"
            )));
        }
        for table in TABLES {
            let present: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |r| r.get(0),
            )?;
            if !present {
                return Err(incompatible(format!("map database lacks table {table}")));
            }
        }
        let meta = |key: &str| -> Result<Option<String>, MapError> {
            Ok(
                c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                    .optional()?,
            )
        };
        if meta("format")?.as_deref() != Some(FORMAT) {
            return Err(incompatible(format!("map format is not {FORMAT}")));
        }
        if meta("relations")?.as_deref() != Some(RELATION_POLICY) {
            return Err(incompatible(format!(
                "map relation policy is not {RELATION_POLICY}; refusing to reinterpret"
            )));
        }
        if meta("ingest")?.as_deref() != Some(super::ingest::INGEST_FORMAT) {
            return Err(incompatible(format!(
                "map ingest format is not {}",
                super::ingest::INGEST_FORMAT
            )));
        }
        if meta("machine_id")?.as_deref() != Some(self.machine.as_str()) {
            return Err(incompatible(
                "map belongs to another machine id; refusing to write as it".to_owned(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

/// Never runs backwards, even if the system clock does.
fn tick(clock: &Clock, tx: &Transaction<'_>) -> Result<i64, MapError> {
    let last: i64 = meta_int(tx, "clock")?;
    let now = clock().max(last);
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'clock'",
        [now.to_string()],
    )?;
    Ok(now)
}

impl MapStore {
    /// Appends a new event of this machine. Returns it with id and hash.
    ///
    /// # Errors
    ///
    /// `invalid_input` for limits/grammar, `forbidden` when the claim needs
    /// local user authority, `not_found`/`conflict` for a bad correction.
    pub fn append(&mut self, draft: EventDraft, authority: Authority) -> Result<Event, MapError> {
        self.write(|w| w.append(draft, authority))
    }

    /// One `BEGIN IMMEDIATE` transaction: every append inside and whatever
    /// else `f` writes (an ingest cursor) commit together or not at all.
    pub(super) fn write<T>(
        &mut self,
        f: impl FnOnce(&mut Writer<'_>) -> Result<T, MapError>,
    ) -> Result<T, MapError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut writer = Writer {
            tx,
            machine: &self.machine,
            clock: &self.clock,
        };
        let out = f(&mut writer)?;
        writer.tx.commit()?;
        Ok(out)
    }

    pub(super) fn reader(&self) -> &Connection {
        &self.connection
    }
}

/// An open write transaction of one store.
pub(super) struct Writer<'a> {
    pub(super) tx: Transaction<'a>,
    machine: &'a str,
    clock: &'a Clock,
}

impl Writer<'_> {
    pub(super) fn now(&self) -> Result<i64, MapError> {
        tick(self.clock, &self.tx)
    }

    pub(super) fn append(
        &mut self,
        draft: EventDraft,
        authority: Authority,
    ) -> Result<Event, MapError> {
        let (tx, clock, machine) = (&self.tx, self.clock, self.machine);
        let mut draft = draft;
        draft.payload = validate_draft(&draft)?;
        check_authority(&draft, authority.as_str())?;
        let seq = meta_int(tx, "next_seq")?;
        if seq > MAX_SEQ {
            return Err(MapError::new(
                MapErrorKind::Conflict,
                "sequence space exhausted",
            ));
        }
        let now = tick(clock, tx)?;
        let mut event = Event {
            v: EVENT_VERSION,
            origin_machine: machine.to_owned(),
            origin_seq: seq,
            authority: authority.as_str().to_owned(),
            recorded_at: now,
            draft,
            content_hash: String::new(),
        };
        event.content_hash = event.compute_hash();
        check_correction(tx, &event)?;
        insert(tx, &event, now)?;
        tx.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'next_seq'",
            [(seq + 1).to_string()],
        )?;
        project_slot(tx, machine, &event.draft)?;
        Ok(event)
    }
}

impl MapStore {
    /// Accepts an event of another origin, exactly as it was recorded there.
    ///
    /// An identical event is a no-op; same id with other content is refused.
    /// Per-origin order is enforced: seq must be the next after the last held.
    ///
    /// # Errors
    ///
    /// `incompatible` for another version, `conflict` for a hash mismatch,
    /// an own-origin event not held, or a gap in the origin's sequence.
    pub fn replay(&mut self, event: &Event) -> Result<Replayed, MapError> {
        if event.v != EVENT_VERSION {
            return Err(MapError::new(
                MapErrorKind::Incompatible,
                format!("event version {} is not {EVENT_VERSION}", event.v),
            ));
        }
        validate_machine(&event.origin_machine)?;
        if !(1..=MAX_SEQ).contains(&event.origin_seq) {
            return Err(MapError::invalid("origin seq out of range"));
        }
        if event.compute_hash() != event.content_hash {
            return Err(MapError::new(
                MapErrorKind::Conflict,
                "content hash does not match the event",
            ));
        }
        // Validity and authority first: an identical copy of a record that should
        // never have been stored is refused, not waved through as a duplicate.
        if validate_draft(&event.draft)? != event.draft.payload {
            return Err(MapError::invalid("payload is not in canonical form"));
        }
        check_authority(&event.draft, &event.authority)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let held: Option<String> = tx
            .query_row(
                "SELECT content_hash FROM events WHERE event_id = ?1",
                [event.event_id()],
                |r| r.get(0),
            )
            .optional()?;
        match held {
            Some(hash) if hash == event.content_hash => return Ok(Replayed::Duplicate),
            Some(_) => {
                return Err(MapError::new(
                    MapErrorKind::Conflict,
                    "an event with this id but other content is held",
                ));
            }
            None => {}
        }
        if event.origin_machine == self.machine {
            return Err(MapError::new(
                MapErrorKind::Conflict,
                "refusing to import an unknown event of this machine",
            ));
        }
        let last: i64 = tx.query_row(
            "SELECT coalesce(max(origin_seq), 0) FROM events WHERE origin_machine = ?1",
            [&event.origin_machine],
            |r| r.get(0),
        )?;
        if event.origin_seq != last + 1 {
            return Err(MapError::new(
                MapErrorKind::Conflict,
                format!(
                    "origin seq {} is not the next after {last}",
                    event.origin_seq
                ),
            ));
        }
        check_correction(&tx, event)?;
        let now = tick(&self.clock, &tx)?;
        insert(&tx, event, now)?;
        project_slot(&tx, &self.machine, &event.draft)?;
        tx.commit()?;
        Ok(Replayed::Inserted)
    }

    /// Events of one space in local receipt order after `cursor` (a `local_seq`),
    /// at most `limit` ≤ 100. The read every caller-facing surface must use.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a limit out of range.
    pub fn events_page(
        &self,
        space: &str,
        cursor: i64,
        limit: usize,
    ) -> Result<(Vec<Event>, Option<i64>), MapError> {
        self.page(
            "space = ?1 AND local_seq > ?2",
            params![space, cursor, sql_limit(limit)?],
            limit,
        )
    }

    /// Every space's events after `cursor`, for trusted replication between this
    /// user's own machines only. Never expose it to an agent, MCP tool or UI.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a limit out of range.
    pub fn trusted_export_page(
        &self,
        cursor: i64,
        limit: usize,
    ) -> Result<(Vec<Event>, Option<i64>), MapError> {
        self.page(
            "?1 = ?1 AND local_seq > ?2",
            params!["all", cursor, sql_limit(limit)?],
            limit,
        )
    }

    fn page(
        &self,
        filter: &str,
        args: impl rusqlite::Params,
        limit: usize,
    ) -> Result<(Vec<Event>, Option<i64>), MapError> {
        let mut stmt = self.connection.prepare(&format!(
            "SELECT {COLUMNS}, local_seq FROM events WHERE {filter} ORDER BY local_seq LIMIT ?3"
        ))?;
        let rows = stmt.query_map(args, |r| Ok((read_event(r)?, r.get::<_, i64>(21)?)))?;
        let mut out: Vec<(Event, i64)> = rows.collect::<Result<_, _>>()?;
        let next = (out.len() > limit).then(|| out[limit - 1].1);
        out.truncate(limit);
        Ok((out.into_iter().map(|(e, _)| e).collect(), next))
    }

    /// Current projection of one space, keyed after `cursor`, at most `limit` ≤ 100.
    ///
    /// # Errors
    ///
    /// `invalid_input` for a limit out of range.
    pub fn links_page(
        &self,
        space: &str,
        cursor: Option<&LinkRow>,
        limit: usize,
    ) -> Result<(Vec<LinkRow>, bool), MapError> {
        self.links_page_of(space, None, cursor, limit)
    }

    /// As [`MapStore::links_page`], optionally only rows of one subject.
    /// The cursor is the last row of the previous page (keyset, stable).
    ///
    /// # Errors
    ///
    /// `invalid_input` for a limit out of range.
    pub fn links_page_of(
        &self,
        space: &str,
        subject: Option<&str>,
        cursor: Option<&LinkRow>,
        limit: usize,
    ) -> Result<(Vec<LinkRow>, bool), MapError> {
        let sql = sql_limit(limit)?;
        let (s, r, o) = cursor.map_or(("", "", ""), |c| {
            (c.subject.as_str(), c.relation.as_str(), c.object.as_str())
        });
        let mut stmt = self.connection.prepare(
            "SELECT space, subject, relation, object, status, contested, resolution, actors, current_events, all_events, \
             best_basis, confidence_milli FROM links WHERE space = ?1 AND (?6 IS NULL OR subject = ?6) \
             AND (subject, relation, object) > (?2, ?3, ?4) ORDER BY subject, relation, object LIMIT ?5",
        )?;
        let mut rows: Vec<LinkRow> = stmt
            .query_map(params![space, s, r, o, sql, subject], read_link)?
            .collect::<Result<_, _>>()?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, more))
    }

    /// What the map knew at local time `as_of` (events received at or before it).
    ///
    /// # Errors
    ///
    /// `invalid_input` for a limit out of range; `conflict` past the projection cap.
    pub fn links_as_of(
        &self,
        space: &str,
        as_of: i64,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LinkRow>, bool), MapError> {
        let limit = page_limit(limit)?;
        let events = load(
            &self.connection,
            "space = ?1 AND received_at <= ?2",
            params![space, as_of],
        )?;
        let rows = project(&events, &self.machine);
        let more = rows.len() > offset.saturating_add(limit);
        Ok((rows.into_iter().skip(offset).take(limit).collect(), more))
    }

    /// Drops and rebuilds `links` from `events`.
    ///
    /// # Errors
    ///
    /// Storage errors; `conflict` past the projection cap.
    pub fn rebuild(&mut self) -> Result<(), MapError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM links", [])?;
        let events = load(&tx, "1 = ?1", params![1])?;
        for row in project(&events, &self.machine) {
            write_link(&tx, &row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Byte-exact dump of the projection, for rebuild comparisons.
    ///
    /// # Errors
    ///
    /// Storage errors.
    pub fn links_dump(&self) -> Result<Vec<u8>, MapError> {
        let mut stmt = self.connection.prepare(
            "SELECT space, subject, relation, object, status, contested, resolution, actors, current_events, all_events, \
             best_basis, confidence_milli FROM links ORDER BY space, subject, relation, object",
        )?;
        let mut out = Vec::new();
        for row in stmt.query_map([], read_link)? {
            out.extend_from_slice(format!("{row:?}\n").as_bytes());
        }
        Ok(out)
    }
}

pub(super) fn page_limit(limit: usize) -> Result<usize, MapError> {
    if (1..=MAX_PAGE).contains(&limit) {
        Ok(limit)
    } else {
        Err(MapError::invalid(format!("limit must be 1..={MAX_PAGE}")))
    }
}

/// Checked `limit + 1` as an SQL integer: one extra row tells whether more follow.
pub(super) fn sql_limit(limit: usize) -> Result<i64, MapError> {
    let limit = page_limit(limit)?;
    i64::try_from(limit + 1).map_err(|_| MapError::invalid("limit out of range"))
}

fn meta_int(tx: &Transaction<'_>, key: &str) -> Result<i64, MapError> {
    let text: String =
        tx.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))?;
    text.parse().map_err(|_| {
        MapError::new(
            MapErrorKind::Incompatible,
            format!("meta {key} is not a number"),
        )
    })
}

/// A correction must name a held event of the same slot and actor that no
/// other correction has already replaced, so chains stay linear.
fn check_correction(tx: &Transaction<'_>, event: &Event) -> Result<(), MapError> {
    let Some(target_id) = &event.draft.corrects else {
        return Ok(());
    };
    parse_event_id(target_id)?;
    let target = load(tx, "event_id = ?1", params![target_id])?
        .pop()
        .ok_or_else(|| MapError::new(MapErrorKind::NotFound, "corrected event is not held"))?;
    let (d, t) = (&event.draft, &target.draft);
    if !(t.kind.starts_with("link_") || t.kind == "correction") {
        return Err(MapError::invalid(
            "only link assertions and corrections can be corrected",
        ));
    }
    if (d.space.as_str(), d.subject.as_str(), d.relation.as_str())
        != (t.space.as_str(), t.subject.as_str(), t.relation.as_str())
    {
        return Err(MapError::invalid(
            "a correction must keep space, subject and relation",
        ));
    }
    if actor(event) != actor(&target) {
        return Err(MapError::new(
            MapErrorKind::Forbidden,
            "only the asserting actor may correct its event",
        ));
    }
    if root_kind(tx, &target)? == "link_confirmed" && event.authority != "local_user" {
        return Err(MapError::new(
            MapErrorKind::Forbidden,
            "correcting a user confirmation needs local user authority",
        ));
    }
    let taken: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE corrects = ?1)",
        [target_id],
        |r| r.get(0),
    )?;
    if taken {
        return Err(MapError::new(
            MapErrorKind::Conflict,
            "event is already corrected; correct the latest correction",
        ));
    }
    Ok(())
}

fn root_kind(tx: &Transaction<'_>, event: &Event) -> Result<String, MapError> {
    let mut current = event.clone();
    while let Some(id) = current.draft.corrects.clone() {
        current = load(tx, "event_id = ?1", params![id])?
            .pop()
            .ok_or_else(|| {
                MapError::new(MapErrorKind::Incompatible, "correction chain is broken")
            })?;
    }
    Ok(current.draft.kind)
}

/// Origin machine plus who asserted: distinct actors are independent sources.
fn actor(e: &Event) -> String {
    format!(
        "{}/{}/{}/{}",
        e.origin_machine, e.draft.asserted_kind, e.draft.client, e.draft.agent
    )
}

fn insert(tx: &Transaction<'_>, e: &Event, received_at: i64) -> Result<(), MapError> {
    let d = &e.draft;
    tx.execute(
        &format!("INSERT INTO events(event_id, received_at, {COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
                  ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)"),
        params![
            e.event_id(), received_at, e.v, e.origin_machine, e.origin_seq, e.authority, e.recorded_at, d.kind,
            d.space, d.subject, d.relation, d.object, d.basis, d.asserted_kind, d.client, d.agent,
            serde_json::to_string(&d.evidence).map_err(|e| MapError::invalid(e.to_string()))?,
            d.occurred_at, d.occurred_at_source, d.confidence_milli, d.corrects, d.payload, e.content_hash
        ],
    )?;
    Ok(())
}

pub(super) fn read_event(r: &Row<'_>) -> rusqlite::Result<Event> {
    let evidence: String = r.get(14)?;
    Ok(Event {
        v: r.get(0)?,
        origin_machine: r.get(1)?,
        origin_seq: r.get(2)?,
        authority: r.get(3)?,
        recorded_at: r.get(4)?,
        draft: EventDraft {
            kind: r.get(5)?,
            space: r.get(6)?,
            subject: r.get(7)?,
            relation: r.get(8)?,
            object: r.get(9)?,
            basis: r.get(10)?,
            asserted_kind: r.get(11)?,
            client: r.get(12)?,
            agent: r.get(13)?,
            evidence: serde_json::from_str(&evidence).unwrap_or_default(),
            occurred_at: r.get(15)?,
            occurred_at_source: r.get(16)?,
            confidence_milli: r.get(17)?,
            corrects: r.get(18)?,
            payload: r.get(19)?,
        },
        content_hash: r.get(20)?,
    })
}

/// Loads events by a fixed filter with bound parameters, in canonical order.
fn load(
    c: &Connection,
    filter: &str,
    args: impl rusqlite::Params + Clone,
) -> Result<Vec<Event>, MapError> {
    load_capped(c, filter, args, MAX_PROJECTION_EVENTS)
}

pub(super) fn load_capped(
    c: &Connection,
    filter: &str,
    args: impl rusqlite::Params + Clone,
    cap: i64,
) -> Result<Vec<Event>, MapError> {
    let count: i64 = c.query_row(
        &format!("SELECT count(*) FROM events WHERE {filter}"),
        args.clone(),
        |r| r.get(0),
    )?;
    if count > cap {
        return Err(MapError::new(
            MapErrorKind::Conflict,
            format!("{count} events exceed the projection cap"),
        ));
    }
    let mut stmt = c.prepare(&format!(
        "SELECT {COLUMNS} FROM events WHERE {filter} ORDER BY origin_machine, origin_seq"
    ))?;
    let rows = stmt.query_map(args, read_event)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) const SLOT_FILTER: &str = "space = ?1 AND subject = ?2 AND relation = ?3";

fn project_slot(tx: &Transaction<'_>, machine: &str, d: &EventDraft) -> Result<(), MapError> {
    if !(d.kind.starts_with("link_") || d.kind == "correction") {
        return Ok(());
    }
    tx.execute(
        &format!("DELETE FROM links WHERE {SLOT_FILTER}"),
        params![d.space, d.subject, d.relation],
    )?;
    // Slot-local: an append re-reads one slot through `events_slot`, never the space.
    let events = load(tx, SLOT_FILTER, params![d.space, d.subject, d.relation])?;
    for row in project(&events, machine) {
        write_link(tx, &row)?;
    }
    Ok(())
}

fn write_link(tx: &Transaction<'_>, row: &LinkRow) -> Result<(), MapError> {
    let json = |v: &Vec<String>| serde_json::to_string(v).unwrap_or_default();
    tx.execute(
        "INSERT INTO links VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            row.space,
            row.subject,
            row.relation,
            row.object,
            row.status,
            row.contested,
            row.resolution,
            json(&row.actors),
            json(&row.current_events),
            json(&row.all_events),
            row.best_basis,
            row.confidence_milli
        ],
    )?;
    Ok(())
}

fn read_link(r: &Row<'_>) -> rusqlite::Result<LinkRow> {
    let list = |i: usize| -> rusqlite::Result<Vec<String>> {
        Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or_default())
    };
    Ok(LinkRow {
        space: r.get(0)?,
        subject: r.get(1)?,
        relation: r.get(2)?,
        object: r.get(3)?,
        status: r.get(4)?,
        contested: r.get(5)?,
        resolution: r.get(6)?,
        actors: list(7)?,
        current_events: list(8)?,
        all_events: list(9)?,
        best_basis: r.get(10)?,
        confidence_milli: r.get(11)?,
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Stance {
    Proposed,
    Confirmed,
    Rejected,
    /// A rejection vouched for on another machine: no authority here.
    Remote,
}

/// Pure projection over events in canonical order. Receipt order and clocks
/// play no part, so the same event set always yields the same rows.
fn project(events: &[Event], machine: &str) -> Vec<LinkRow> {
    let by_id: BTreeMap<String, &Event> = events.iter().map(|e| (e.event_id(), e)).collect();
    let superseded: BTreeSet<&str> = events
        .iter()
        .filter_map(|e| e.draft.corrects.as_deref())
        .collect();
    let mut slots: BTreeMap<(&str, &str, &str), Vec<&Event>> = BTreeMap::new();
    for e in events
        .iter()
        .filter(|e| e.draft.kind.starts_with("link_") || e.draft.kind == "correction")
    {
        slots
            .entry((&e.draft.space, &e.draft.subject, &e.draft.relation))
            .or_default()
            .push(e);
    }
    let stance = |e: &Event| {
        let mut root = e;
        while let Some(prev) = root.draft.corrects.as_deref().and_then(|id| by_id.get(id)) {
            root = prev;
        }
        let local = root.authority == "local_user" && root.origin_machine == machine;
        match (root.draft.kind.as_str(), local) {
            ("link_confirmed", true) => Stance::Confirmed,
            ("link_rejected", true) => Stance::Rejected,
            // A confirmation vouched for elsewhere is only a claim here.
            ("link_confirmed", false) => Stance::Proposed,
            ("link_rejected", false) => Stance::Remote,
            _ => Stance::Proposed,
        }
    };
    let rank = |basis: &str| {
        BASES
            .iter()
            .position(|(b, _)| *b == basis)
            .unwrap_or(usize::MAX)
    };
    let mut out = Vec::new();
    for ((space, subject, relation), slot) in slots {
        let live: Vec<&Event> = slot
            .iter()
            .copied()
            .filter(|e| !superseded.contains(e.event_id().as_str()) && !e.draft.object.is_empty())
            .collect();
        let row = |object: &str, status: &str, contested: bool, resolved: bool| {
            let all: Vec<&Event> = slot
                .iter()
                .copied()
                .filter(|e| e.draft.object == object)
                .collect();
            let current: Vec<&Event> = live
                .iter()
                .copied()
                .filter(|e| e.draft.object == object)
                .collect();
            let shown = if current.is_empty() { &all } else { &current };
            let mut actors: Vec<String> = shown.iter().map(|e| actor(e)).collect();
            actors.sort();
            actors.dedup();
            let best = shown
                .iter()
                .min_by_key(|e| (rank(&e.draft.basis), e.event_id()))
                .map_or("", |e| e.draft.basis.as_str());
            LinkRow {
                space: space.to_owned(),
                subject: subject.to_owned(),
                relation: relation.to_owned(),
                object: object.to_owned(),
                status: status.to_owned(),
                contested,
                resolution: if resolved { "user_confirmed" } else { "none" }.to_owned(),
                actors,
                current_events: current.iter().map(|e| e.event_id()).collect(),
                all_events: all.iter().map(|e| e.event_id()).collect(),
                best_basis: best.to_owned(),
                confidence_milli: shown
                    .iter()
                    .map(|e| i64::from(e.draft.confidence_milli))
                    .max()
                    .unwrap_or(0),
            }
        };
        let objects: BTreeSet<&str> = slot
            .iter()
            .map(|e| e.draft.object.as_str())
            .filter(|o| !o.is_empty())
            .collect();
        // Every stored relation passed the policy; fall back to the strict reading.
        if relation_policy(relation).map_or(Cardinality::Single, |(c, _)| c) == Cardinality::Multi {
            // Each object is its own association: only claims about it matter.
            for object in objects {
                let has = |s: Stance| {
                    live.iter()
                        .any(|e| e.draft.object == object && stance(e) == s)
                };
                let (confirmed, rejected) = (has(Stance::Confirmed), has(Stance::Rejected));
                let status = match (confirmed, rejected, has(Stance::Proposed)) {
                    (true, true, _) => "conflict",
                    (false, true, _) => "rejected",
                    (true, false, _) => "confirmed",
                    (false, false, true) => "proposed",
                    (false, false, false) => "superseded",
                };
                out.push(row(
                    object,
                    status,
                    confirmed && rejected,
                    confirmed != rejected,
                ));
            }
            continue;
        }
        let pick = |s: Stance| {
            live.iter()
                .filter(|e| stance(e) == s)
                .map(|e| e.draft.object.as_str())
                .collect::<BTreeSet<_>>()
        };
        let (confirmed, rejected) = (pick(Stance::Confirmed), pick(Stance::Rejected));
        // Every assertion ever made counts, so a later correction cannot erase
        // that two independent sources disagreed.
        let mut claims: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for e in slot
            .iter()
            .filter(|e| !e.draft.object.is_empty() && stance(e) == Stance::Proposed)
        {
            if !rejected.contains(e.draft.object.as_str()) {
                claims
                    .entry(e.draft.object.as_str())
                    .or_default()
                    .insert(actor(e));
            }
        }
        let actors_total: BTreeSet<&String> = claims.values().flatten().collect();
        let live_values: BTreeSet<&str> = live
            .iter()
            .filter(|e| {
                stance(e) == Stance::Proposed && !rejected.contains(e.draft.object.as_str())
            })
            .map(|e| e.draft.object.as_str())
            .collect();
        let contested = (claims.len() > 1 && actors_total.len() > 1) || live_values.len() > 1;
        let resolved = confirmed.len() == 1 && rejected.is_disjoint(&confirmed);
        for object in objects {
            let is_current = live
                .iter()
                .any(|e| e.draft.object == object && stance(e) != Stance::Remote)
                || confirmed.contains(object);
            let status = if confirmed.contains(object) && rejected.contains(object) {
                "conflict"
            } else if rejected.contains(object) {
                "rejected"
            } else if confirmed.contains(object) {
                if confirmed.len() > 1 {
                    "conflict"
                } else {
                    "confirmed"
                }
            } else if resolved {
                "overridden"
            } else if !is_current {
                "superseded"
            } else if contested || confirmed.len() > 1 {
                "conflict"
            } else {
                "proposed"
            };
            out.push(row(object, status, contested, resolved));
        }
    }
    out
}
