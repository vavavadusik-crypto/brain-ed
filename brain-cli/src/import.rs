//! Explicit graph import with provenance and bounded validation.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;

use brain_core::{
    Attribution, NewEdge, NewNode, NodeKind, NodeReplacement, SqliteStorage, Storage, Timestamp,
};
use clap::Args;
use serde_json::{Value, json};

/// Prefix marking a record that came from the producer rather than an agent.
///
/// It is what makes "refresh the imported part, keep everything else" a rule
/// the code can follow rather than a promise in a comment.
const IMPORTED_PREFIX: &str = "g:";

#[derive(Debug, Args)]
pub(crate) struct ImportArgs {
    /// Path to the producer's export, or the directory holding it.
    #[arg(value_name = "PATH")]
    path: std::path::PathBuf,

    /// Which producer wrote it.
    #[arg(long, default_value = "graphify", value_parser = ["graphify", "hermes", "markdown"])]
    from: String,

    /// Report what would change and write nothing.
    #[arg(long)]
    dry_run: bool,

    /// Add a tag to every Markdown document and section (repeatable).
    #[arg(long = "tag")]
    tags: Vec<String>,

    /// Skip Markdown directories with this name anywhere below PATH
    /// (repeatable), e.g. a vault folder generated from another source.
    #[arg(long = "exclude", value_name = "DIR_NAME")]
    exclude: Vec<String>,
}

/// One side-by-side comparison of the export against what is stored.
struct Plan {
    new_nodes: usize,
    changed_nodes: usize,
    unchanged_nodes: usize,
    new_edges: usize,
    /// Records this memory owns, which an import must leave alone.
    owned_untouched: usize,
    /// Imported records the export no longer mentions.
    ///
    /// Reported rather than deleted: an export truncated by a failed run would
    /// otherwise quietly take real history with it.
    missing_from_export: usize,
}

pub(crate) fn run(config: &crate::RuntimeConfig, args: &ImportArgs) -> Result<(), Box<dyn Error>> {
    if args.from == "markdown" {
        return markdown::run(config, args);
    }
    if args.from == "hermes" {
        return import_hermes(config, args);
    }
    let file = resolve_export(&args.path)?;
    let export = read_export(&file, MAX_EXPORT_BYTES)?;
    let format = export["format"].as_str().unwrap_or("");
    if !format.starts_with("brain-data") {
        return Err(format!(
            "{} is not a Graphify export (format: {format:?})",
            file.display()
        )
        .into());
    }

    let nodes = export["nodes"].as_array().cloned().unwrap_or_default();
    let links = export["links"].as_array().cloned().unwrap_or_default();
    // Refused before anything is opened for writing, so an oversized export
    // never leaves a partial import behind.
    if nodes.len() > MAX_EXPORT_NODES || links.len() > MAX_EXPORT_LINKS {
        return Err(format!(
            "{} holds {} nodes and {} links; the import limit is {MAX_EXPORT_NODES} nodes and \
             {MAX_EXPORT_LINKS} links, nothing was written",
            file.display(),
            nodes.len(),
            links.len()
        )
        .into());
    }
    let types = export["dictionaries"]["types"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|value| value.as_str().unwrap_or("concept").to_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let storage = crate::open_storage(&config.database)?;
    let now = crate::now_unix_seconds();
    let stored = existing_records(&config.database, &config.space)?;

    let mut plan = Plan {
        new_nodes: 0,
        changed_nodes: 0,
        unchanged_nodes: 0,
        new_edges: 0,
        owned_untouched: stored
            .labels
            .keys()
            .filter(|id| !id.starts_with(IMPORTED_PREFIX))
            .count(),
        missing_from_export: 0,
    };

    let mut incoming = Vec::with_capacity(nodes.len());
    let mut seen = HashSet::with_capacity(nodes.len());
    for node in &nodes {
        let Some(fields) = node.as_array() else {
            continue;
        };
        let Some(source_id) = fields.first().and_then(Value::as_str) else {
            continue;
        };
        let id = format!("{IMPORTED_PREFIX}{source_id}");
        // Masked like every other source before it is compared or stored: a credential
        // in a graph label must not reach the brain, and a label already masked in place
        // must not look changed on the next import.
        let label =
            crate::redact::mask_secrets(fields.get(1).and_then(Value::as_str).unwrap_or(source_id))
                .0;
        let kind = fields
            .get(2)
            .and_then(Value::as_u64)
            .and_then(|index| types.get(index as usize))
            .map_or(NodeKind::Concept, |name| kind_from(name));
        match stored.labels.get(&id) {
            Some(current) if *current == label => plan.unchanged_nodes += 1,
            Some(_) => plan.changed_nodes += 1,
            None => plan.new_nodes += 1,
        }
        seen.insert(id.clone());
        incoming.push((id, label, kind));
    }

    plan.missing_from_export = stored
        .labels
        .keys()
        .filter(|id| id.starts_with(IMPORTED_PREFIX) && !seen.contains(*id))
        .count();

    let edges = collect_edges(&links, &nodes, &stored.edges, &mut plan);

    if args.dry_run {
        return crate::print_json(&report("planned", &file, &plan, 0, 0));
    }

    // A new version must be recorded strictly after the one it replaces; SQLite time has
    // one-second resolution, so a re-import in the same second waits for the next one.
    let now = if plan.changed_nodes > 0 {
        after_latest_recording(&config.database, &config.space, now)?
    } else {
        now
    };
    let attribution = crate::ensure_context(&storage, config, now)?;
    let written_nodes = write_nodes(
        &storage,
        &config.space,
        &incoming,
        &stored.labels,
        now,
        &attribution,
    )?;
    let written_edges = write_edges(&storage, &config.space, &edges, now, &attribution)?;
    storage.flush()?;

    crate::print_json(&report("ok", &file, &plan, written_nodes, written_edges))?;
    Ok(())
}

fn report(
    status: &str,
    file: &Path,
    plan: &Plan,
    written_nodes: usize,
    written_edges: usize,
) -> Value {
    report_for(
        status,
        file,
        plan,
        written_nodes,
        written_edges,
        IMPORTED_PREFIX,
    )
}

fn report_for(
    status: &str,
    file: &Path,
    plan: &Plan,
    written_nodes: usize,
    written_edges: usize,
    prefix: &str,
) -> Value {
    json!({
        "status": status,
        "source": file,
        "nodes": {
            "new": plan.new_nodes,
            "changed": plan.changed_nodes,
            "unchanged": plan.unchanged_nodes,
            "written": written_nodes,
        },
        "edges": {
            "new": plan.new_edges,
            "written": written_edges,
        },
        "own_memory_untouched": plan.owned_untouched,
        "imported_missing_from_export": plan.missing_from_export,
        "note": format!(
            "импорт обновляет только записи с префиксом {prefix}; собственная память агентов не затрагивается"
        ),
    })
}

/// Finds the export file, accepting either the file or its directory.
/// Largest Graphify export read; the file is never buffered past this.
const MAX_EXPORT_BYTES: u64 = 256 * 1024 * 1024;
/// Most nodes and links one Graphify import accepts.
const MAX_EXPORT_NODES: usize = 1_000_000;
const MAX_EXPORT_LINKS: usize = 4_000_000;

/// Reads and parses an export, refusing one over `max_bytes` without ever
/// holding more than `max_bytes + 1` of it.
fn read_export(file: &Path, max_bytes: u64) -> Result<Value, Box<dyn Error>> {
    use std::io::Read;
    let mut raw = Vec::new();
    fs::File::open(file)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut raw)?;
    if raw.len() as u64 > max_bytes {
        return Err(format!(
            "{} is larger than the {max_bytes}-byte import limit; nothing was written",
            file.display()
        )
        .into());
    }
    Ok(serde_json::from_slice(&raw)?)
}

fn resolve_export(path: &Path) -> Result<std::path::PathBuf, Box<dyn Error>> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let candidate = path.join("brain-data.json");
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(format!(
        "no Graphify export at {}; expected the file itself or a directory containing brain-data.json",
        path.display()
    )
    .into())
}

struct Existing {
    labels: HashMap<String, String>,
    /// Connections already stored, keyed by the pair they join.
    ///
    /// Keying by our own generated identifier instead would have called every
    /// one of the 24,533 exported links "new" against a database already
    /// holding 31,131 of them — the same connections under identifiers this
    /// importer had not issued, duplicated on first run.
    edges: HashSet<(String, String)>,
}

/// Reads what is already stored, so an import can tell new from changed.
fn existing_records(database: &Path, space: &str) -> Result<Existing, Box<dyn Error>> {
    let connection = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let mut statement = connection.prepare(
        r#"SELECT v.node_id, v.label
           FROM node_versions AS v
           WHERE v.space_id = ?1 AND v.superseded_at IS NULL"#,
    )?;
    let labels = statement
        .query_map([space], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    drop(statement);

    let mut statement =
        connection.prepare("SELECT from_node_id, to_node_id FROM edges WHERE space_id = ?1")?;
    let edges = statement
        .query_map([space], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(Existing { labels, edges })
}

/// Turns index pairs into edges, skipping ones already stored.
fn collect_edges(
    links: &[Value],
    nodes: &[Value],
    stored: &HashSet<(String, String)>,
    plan: &mut Plan,
) -> Vec<(String, String, String)> {
    let ids = nodes
        .iter()
        .map(|node| {
            node.as_array()
                .and_then(|fields| fields.first())
                .and_then(Value::as_str)
                .map(|id| format!("{IMPORTED_PREFIX}{id}"))
        })
        .collect::<Vec<_>>();

    let mut edges = Vec::new();
    for link in links {
        let Some(fields) = link.as_array() else {
            continue;
        };
        let (Some(from), Some(to)) = (
            fields.first().and_then(Value::as_u64),
            fields.get(1).and_then(Value::as_u64),
        ) else {
            continue;
        };
        let (Some(Some(from_id)), Some(Some(to_id))) =
            (ids.get(from as usize), ids.get(to as usize))
        else {
            continue;
        };
        // Matched on the pair, not on an identifier we invented: the same two
        // records joined is the same connection, whoever wrote it and whatever
        // they called it.
        if stored.contains(&(from_id.clone(), to_id.clone())) {
            continue;
        }
        // A deterministic identifier is what makes a re-import an update rather
        // than a duplicate: the same pair always produces the same edge.
        let id = format!("gx:{from_id}->{to_id}");
        plan.new_edges += 1;
        edges.push((id, from_id.clone(), to_id.clone()));
    }
    edges
}

fn after_latest_recording(
    database: &Path,
    space: &str,
    now: Timestamp,
) -> Result<Timestamp, Box<dyn Error>> {
    let db = rusqlite::Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let latest: i64 = db.query_row(
        "SELECT coalesce(max(recorded_at), 0) FROM node_versions WHERE space_id = ?1",
        [space],
        |row| row.get(0),
    )?;
    if latest > crate::now_unix_seconds() + 2 {
        return Err(
            "stored recording time is in the future; retry after the clock catches up".into(),
        );
    }
    let mut now = now.max(crate::now_unix_seconds());
    while now <= latest {
        std::thread::sleep(std::time::Duration::from_millis(20));
        now = crate::now_unix_seconds();
    }
    Ok(now)
}

fn write_nodes(
    storage: &SqliteStorage,
    space: &str,
    incoming: &[(String, String, NodeKind)],
    stored: &HashMap<String, String>,
    now: Timestamp,
    attribution: &Attribution,
) -> Result<usize, Box<dyn Error>> {
    let metadata_json = json!({"imported_from": "graphify"}).to_string();
    let mut fresh = Vec::new();
    let mut changed = 0;
    for (id, label, kind) in incoming {
        match stored.get(id) {
            Some(current) if current == label => {}
            // A node the brain already holds gets a new version; inserting it again
            // would collide with its identity and abort the whole import.
            Some(_) => {
                storage.replace_node(
                    space,
                    id,
                    &NodeReplacement {
                        kind: *kind,
                        label: label.clone(),
                        body: label.clone(),
                        source: Some("graphify".to_owned()),
                        metadata_json: metadata_json.clone(),
                        valid_from: now,
                        attribution: attribution.clone(),
                    },
                )?;
                changed += 1;
            }
            None => fresh.push(NewNode {
                id: id.clone(),
                space_id: space.to_owned(),
                kind: *kind,
                label: label.clone(),
                body: label.clone(),
                source: Some("graphify".to_owned()),
                metadata_json: metadata_json.clone(),
                valid_from: now,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 0.6,
                source_weight: 0.9,
            }),
        }
    }
    if !fresh.is_empty() {
        storage.insert_nodes(&fresh)?;
    }
    Ok(fresh.len() + changed)
}

fn write_edges(
    storage: &SqliteStorage,
    space: &str,
    edges: &[(String, String, String)],
    now: Timestamp,
    attribution: &Attribution,
) -> Result<usize, Box<dyn Error>> {
    if edges.is_empty() {
        return Ok(0);
    }
    let batch = edges
        .iter()
        .map(|(id, from, to)| NewEdge {
            id: id.clone(),
            space_id: space.to_owned(),
            from_node_id: from.clone(),
            to_node_id: to.clone(),
            relation: "relates_to".to_owned(),
            weight: 0.7,
            confidence: 0.8,
            metadata_json: json!({"imported_from": "graphify"}).to_string(),
            valid_from: now,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 0.6,
        })
        .collect::<Vec<_>>();
    storage.insert_edges(&batch)?;
    Ok(batch.len())
}

/// Prefix marking a record that came from the Hermes operational brain.
///
/// Separate from the Graphify prefix so refreshing one producer cannot touch
/// what the other contributed, and neither can touch the agents' own memory.
const HERMES_PREFIX: &str = "h:";

/// Pulls entities and decisions out of a Hermes brain database.
///
/// Relations are deliberately left behind on the first pass: there are 474,350
/// of them against 156,675 entities, and importing a graph six times denser
/// than the one already stored would bury it rather than enrich it. Entities
/// and the decisions attached to them are the part with lasting value.
fn import_hermes(config: &crate::RuntimeConfig, args: &ImportArgs) -> Result<(), Box<dyn Error>> {
    let source = if args.path.is_file() {
        args.path.clone()
    } else {
        args.path.join("brain.sqlite")
    };
    if !source.is_file() {
        return Err(format!("no Hermes database at {}", source.display()).into());
    }

    let hermes = open_hermes_source(&source)?;
    let rows = hermes_rows(&hermes, MAX_HERMES_ROWS)?;
    let stored = existing_records(&config.database, &config.space)?;

    let mut plan = Plan {
        new_nodes: 0,
        changed_nodes: 0,
        unchanged_nodes: 0,
        new_edges: 0,
        owned_untouched: stored
            .labels
            .keys()
            .filter(|id| !id.starts_with(IMPORTED_PREFIX) && !id.starts_with(HERMES_PREFIX))
            .count(),
        missing_from_export: 0,
    };

    let incoming = rows
        .into_iter()
        .map(|(stable_id, kind, label)| {
            let id = format!("{HERMES_PREFIX}{stable_id}");
            match stored.labels.get(&id) {
                Some(current) if *current == label => plan.unchanged_nodes += 1,
                Some(_) => plan.changed_nodes += 1,
                None => plan.new_nodes += 1,
            }
            (id, label, hermes_kind(&kind))
        })
        .collect::<Vec<_>>();

    if args.dry_run {
        return crate::print_json(&report_for("planned", &source, &plan, 0, 0, HERMES_PREFIX));
    }

    let now = crate::now_unix_seconds();
    let storage = crate::open_storage(&config.database)?;
    let attribution = crate::ensure_context(&storage, config, now)?;
    let written = write_nodes(
        &storage,
        &config.space,
        &incoming,
        &stored.labels,
        now,
        &attribution,
    )?;
    storage.flush()?;

    crate::print_json(&report_for("ok", &source, &plan, written, 0, HERMES_PREFIX))?;
    Ok(())
}

/// Most Hermes entities one import accepts; a larger selection is refused.
const MAX_HERMES_ROWS: usize = 500_000;
/// Longest stable id and label accepted from a Hermes row, in bytes.
const MAX_HERMES_ID_BYTES: usize = 1_024;
const MAX_HERMES_LABEL_BYTES: usize = 16 * 1024;

/// Opens a Hermes database as a read-only personal source.
///
/// The file and any WAL side files must be regular, not symlinks, owned by
/// this OS user and not writable by group or others (a read-only 0644 source
/// is fine: reading it discloses nothing new). Nothing about the source or its
/// directory is changed. SQLite's normal read locks apply.
fn open_hermes_source(source: &Path) -> Result<rusqlite::Connection, Box<dyn Error>> {
    check_private_source(source)?;
    for suffix in ["-wal", "-shm"] {
        let side = std::path::PathBuf::from(format!("{}{suffix}", source.display()));
        match fs::symlink_metadata(&side) {
            Ok(_) => check_private_source(&side)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let connection = rusqlite::Connection::open_with_flags(
        source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.pragma_update(None, "query_only", true)?;
    for table in ["entities", "decisions", "claims"] {
        let kind: Option<String> = connection
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = ?1",
                [table],
                |row| row.get(0),
            )
            .ok();
        if kind.as_deref() != Some("table") {
            return Err(format!(
                "{} is not a Hermes database: `{table}` must be a table, found {}",
                source.display(),
                kind.as_deref().unwrap_or("nothing")
            )
            .into());
        }
    }
    Ok(connection)
}

fn check_private_source(path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(format!("{} must be a regular file, not a symlink", path.display()).into());
    }
    // An unknown process owner trusts nothing.
    let owner = crate::private_fs::current_uid();
    if owner != Some(meta.uid()) || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be owned by this user and not group/other writable (chmod go-w it \
             yourself); nothing was read",
            path.display()
        )
        .into());
    }
    Ok(())
}

/// A Hermes entity: stable id, type and label.
type HermesRow = (String, String, String);

/// Selects the importable entities, reading at most `cap + 1` rows so an
/// oversized source is refused, never truncated silently.
fn hermes_rows(
    hermes: &rusqlite::Connection,
    cap: usize,
) -> Result<Vec<HermesRow>, Box<dyn Error>> {
    // Only entities that carry a decision or a claim: the rest are structural
    // rows describing files and symbols, which the code indexer already covers
    // and which would add six figures of noise.
    let mut statement = hermes.prepare(
        r#"SELECT e.stable_id, e.type, e.label
           FROM entities AS e
           WHERE e.stable_id IN (SELECT subject_id FROM decisions)
              OR e.stable_id IN (SELECT subject_id FROM claims)
              OR e.type IN ('DECISION', 'BLOCKER', 'CHECKPOINT', 'RUNBOOK')
           LIMIT ?1"#,
    )?;
    let mut rows = statement.query([i64::try_from(cap.saturating_add(1)).unwrap_or(i64::MAX)])?;
    let mut selected = Vec::new();
    while let Some(row) = rows.next()? {
        if selected.len() >= cap {
            return Err(format!(
                "the Hermes source selects more than {cap} entities; nothing was written"
            )
            .into());
        }
        let field = |index: usize, max: usize| -> Result<String, Box<dyn Error>> {
            let value = row.get_ref(index)?;
            let bytes = value.as_bytes().map_err(|_| "Hermes field is not text")?;
            if bytes.len() > max {
                return Err(
                    format!("a Hermes field exceeds {max} bytes; nothing was written").into(),
                );
            }
            Ok(std::str::from_utf8(bytes)?.to_owned())
        };
        selected.push((
            field(0, MAX_HERMES_ID_BYTES)?,
            field(1, MAX_HERMES_ID_BYTES)?,
            field(2, MAX_HERMES_LABEL_BYTES)?,
        ));
    }
    Ok(selected)
}

/// Maps a Hermes entity type onto a kind this memory understands.
fn hermes_kind(kind: &str) -> NodeKind {
    match kind {
        "DECISION" => NodeKind::Decision,
        "BLOCKER" | "INCIDENT" => NodeKind::Error,
        "CHECKPOINT" | "SESSION" => NodeKind::Session,
        "RUNBOOK" | "DOCUMENT" => NodeKind::Document,
        "FILE" | "SYMBOL" | "MODULE" => NodeKind::Code,
        _ => NodeKind::Entity,
    }
}

fn kind_from(name: &str) -> NodeKind {
    match name {
        "code" => NodeKind::Code,
        "decision" | "rationale" => NodeKind::Decision,
        "document" => NodeKind::Document,
        "error" => NodeKind::Error,
        "memory" => NodeKind::Memory,
        "session" => NodeKind::Session,
        "entity" => NodeKind::Entity,
        _ => NodeKind::Concept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole safety of a re-import rests on the prefix: an agent's own
    /// memory must never be a candidate for replacement by the producer.
    #[test]
    fn only_imported_records_are_candidates() {
        assert!("g:memory_something".starts_with(IMPORTED_PREFIX));
        assert!(!"mem_08949b4bf8c758f2".starts_with(IMPORTED_PREFIX));
    }

    #[test]
    fn an_edge_identifier_is_the_same_every_import() {
        let first = format!("gx:{}->{}", "g:a", "g:b");
        let again = format!("gx:{}->{}", "g:a", "g:b");
        let reversed = format!("gx:{}->{}", "g:b", "g:a");

        assert_eq!(first, again, "a re-import must update, not duplicate");
        assert_ne!(first, reversed, "direction is part of the edge");
    }

    #[test]
    fn unknown_kinds_become_concepts_rather_than_failing() {
        assert_eq!(kind_from("code"), NodeKind::Code);
        assert_eq!(kind_from("rationale"), NodeKind::Decision);
        assert_eq!(
            kind_from("something-new-from-a-later-graphify"),
            NodeKind::Concept
        );
    }

    fn scratch(_label: &str) -> std::path::PathBuf {
        let dir = crate::test_support::temp_path("brain-import");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn hermes_fixture(path: &Path, entities: usize) {
        use std::os::unix::fs::PermissionsExt;
        let db = rusqlite::Connection::open(path).expect("hermes");
        db.execute_batch(
            "CREATE TABLE entities(stable_id TEXT, type TEXT, label TEXT);
             CREATE TABLE decisions(subject_id TEXT);
             CREATE TABLE claims(subject_id TEXT);
             INSERT INTO entities VALUES ('file1', 'FILE', 'src/main.rs');",
        )
        .expect("schema");
        for index in 0..entities {
            db.execute(
                "INSERT INTO entities VALUES (?1, 'DECISION', ?2)",
                [format!("d{index}"), format!("решение {index}")],
            )
            .expect("row");
        }
        drop(db);
        // Ordinary Hermes files are 0644: accepted, since they are not writable by others.
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("mode");
    }

    #[test]
    fn a_private_hermes_source_is_read_within_its_cap() {
        let dir = scratch("hermes-ok");
        let source = dir.join("brain.sqlite");
        hermes_fixture(&source, 3);
        let before = fs::read(&source).expect("before");
        let hermes = open_hermes_source(&source).expect("open");
        let rows = hermes_rows(&hermes, 3).expect("rows");
        assert_eq!(rows.len(), 3, "structural FILE rows stay out");
        assert!(rows.iter().any(|row| row.2 == "решение 1"));
        let refused = hermes_rows(&hermes, 2).expect_err("over cap").to_string();
        assert!(refused.contains("more than 2"), "{refused}");
        drop(hermes);
        assert_eq!(
            fs::read(&source).expect("after"),
            before,
            "source unchanged"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsafe_hermes_source_is_refused_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("hermes-unsafe");
        let source = dir.join("brain.sqlite");
        hermes_fixture(&source, 1);
        let link = dir.join("link.sqlite");
        std::os::unix::fs::symlink(&source, &link).expect("symlink");
        assert!(open_hermes_source(&link).is_err());
        for mode in [0o666, 0o664, 0o646] {
            fs::set_permissions(&source, fs::Permissions::from_mode(mode)).expect("chmod");
            let before = fs::read(&source).expect("before");
            assert!(open_hermes_source(&source).is_err(), "{mode:o} refused");
            let kept = fs::metadata(&source).expect("meta").permissions().mode() & 0o777;
            assert_eq!(kept, mode, "the source mode is never changed");
            assert_eq!(fs::read(&source).expect("after"), before);
        }
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).expect("chmod");
        let db = rusqlite::Connection::open(&source).expect("db");
        db.execute_batch("DROP TABLE claims; CREATE VIEW claims AS SELECT 'd0' AS subject_id;")
            .expect("view");
        drop(db);
        let refused = open_hermes_source(&source).expect_err("view").to_string();
        assert!(refused.contains("`claims` must be a table"), "{refused}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_export_is_refused_before_parsing() {
        let dir = scratch("export-cap");
        let file = dir.join("brain-data.json");
        let text = r#"{"format":"brain-data/1","nodes":[["n1","Узел",0]],"links":[]}"#;
        fs::write(&file, text).expect("write");
        let limit = text.len() as u64;
        assert_eq!(
            read_export(&file, limit).expect("fits")["nodes"][0][1],
            "Узел"
        );
        let refused = read_export(&file, limit - 1).expect_err("over").to_string();
        assert!(refused.contains("import limit"), "{refused}");
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Markdown is imported as untrusted, searchable document text. No templates,
/// YAML constructors, embedded commands or remote links are ever evaluated.
mod markdown {
    use super::*;
    use rusqlite::{Connection, OptionalExtension, params};
    use std::io::Read;
    use std::path::{Component, PathBuf};
    use std::time::UNIX_EPOCH;

    const MAX_BYTES: u64 = 1024 * 1024;
    /// Documents above this size are split into sections: a whole mid-size file is one long
    /// node that ranks poorly and, above the search budget, can never be returned at all.
    const SECTION_BYTES: usize = 4 * 1024;
    /// A section above this size is cut further, at line boundaries, into parts of
    /// at most PART_BYTES.
    const SECTION_MAX_BYTES: usize = 6 * 1024;
    const PART_BYTES: usize = 4 * 1024;
    /// Text before the first heading becomes its own section when it says this much.
    const INTRO_MIN_BYTES: usize = 200;
    /// Stored with every document: a new value re-imports unchanged files once, so
    /// sections already stored are cut again by the current rules.
    const SECTIONING: &str = "sections:2";

    #[derive(Default)]
    struct Report {
        files_seen: usize,
        imported: usize,
        updated: usize,
        unchanged: usize,
        skipped: Vec<Value>,
        warnings: Vec<Value>,
        edges: usize,
    }

    impl Report {
        fn skip(&mut self, path: &Path, reason: &str) {
            self.skipped
                .push(json!({"path": path.to_string_lossy(), "reason": reason}));
        }
        fn warn(&mut self, path: &Path, reason: &str) {
            self.warnings
                .push(json!({"path": path.to_string_lossy(), "reason": reason}));
        }
        fn json(&self, dry_run: bool) -> Value {
            json!({"status": if dry_run { "planned" } else { "ok" },
                "files_seen": self.files_seen, "imported": self.imported,
                "updated": self.updated, "unchanged": self.unchanged,
                "skipped": self.skipped, "edges": self.edges, "warnings": self.warnings})
        }
    }

    struct Document {
        path: PathBuf,
        id: String,
        hash: String,
        label: String,
        body: String,
        metadata: Value,
        time: i64,
        sections: Vec<(String, String)>,
    }

    pub(super) fn run(
        config: &crate::RuntimeConfig,
        args: &ImportArgs,
    ) -> Result<(), Box<dyn Error>> {
        let input = if args.path.is_absolute() {
            args.path.clone()
        } else {
            std::env::current_dir()?.join(&args.path)
        };
        // Check the requested spelling as well as resolved paths: a symlink
        // must never turn a forbidden source into an apparently harmless one.
        let mut report = Report::default();
        if secret_path(&input) {
            report.skip(&input, "secret_path");
            return crate::print_json(&report.json(args.dry_run));
        }
        let canonical = match input.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                report.warn(&input, "source_missing; stored history was retained");
                return crate::print_json(&report.json(args.dry_run));
            }
            Err(e) => return Err(e.into()),
        };
        let directory = canonical.is_dir();
        let root = if directory {
            canonical.clone()
        } else {
            input
                .parent()
                .ok_or("source has no parent")?
                .canonicalize()?
        };
        let mut files = Vec::new();
        walk(
            &input,
            &root,
            &args.exclude,
            &mut HashSet::new(),
            &mut files,
            &mut report,
        );
        files.sort();
        files.dedup();
        let clock = Connection::open_in_memory()?;
        let mut docs = Vec::new();
        for file in files {
            match read_document(&file, &root, &args.tags, &clock, &mut report) {
                Ok(Some(doc)) => docs.push(doc),
                Ok(None) => {}
                Err(_) => report.skip(&file, "unreadable_or_invalid_utf8"),
            }
        }

        // In dry-run even a missing DB parent stays absent. The existing DB is
        // opened read-only; no schema migrations or context/session writes run.
        let existing = if config.database.exists() {
            Some(Connection::open_with_flags(
                &config.database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?)
        } else {
            None
        };
        let links = links(&docs, &root, &mut report);
        let changed = plan(existing.as_ref(), &config.space, &mut docs, &mut report)?;
        if let Some(db) = existing.as_ref() {
            report_missing(db, &config.space, &canonical, directory, &mut report)?;
        }
        for (from, to, relation, index) in &links {
            if changed.contains(index)
                || !edge_current(
                    existing.as_ref(),
                    &config.space,
                    &edge_id(from, to, relation),
                )?
            {
                report.edges += 1;
            }
        }
        if args.dry_run || (changed.is_empty() && report.edges == 0) {
            return crate::print_json(&report.json(args.dry_run));
        }
        drop(existing);
        let storage = crate::open_storage(&config.database)?;
        let attribution = crate::ensure_context(&storage, config, crate::now_unix_seconds())?;
        storage.flush()?;
        drop(storage);
        let mut db = Connection::open(&config.database)?;
        db.pragma_update(None, "foreign_keys", true)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Re-plan under the write lock, so concurrent imports cannot duplicate
        // revisions. Recording time advances independently of source event time.
        let changed = plan(Some(&tx), &config.space, &mut docs, &mut Report::default())?;
        let latest: i64 = tx.query_row(
            "SELECT max(coalesce((SELECT max(recorded_at) FROM node_versions WHERE space_id=?1),0),
                        coalesce((SELECT max(recorded_at) FROM edge_versions WHERE space_id=?1),0))",
            [&config.space], |r| r.get(0))?;
        // SQLite timestamps have one-second resolution. Do not invent a future
        // recording time: an immediate `brain ask` would miss that assertion.
        if latest > crate::now_unix_seconds() + 2 {
            return Err(
                "stored recording time is in the future; retry after the clock catches up".into(),
            );
        }
        while crate::now_unix_seconds() <= latest {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let recorded = crate::now_unix_seconds();
        for &index in &changed {
            let doc = &docs[index];
            // Retire the previous file assertion and all its sections together.
            // This supports date corrections in either direction and retains all
            // previous versions for recorded-time queries, including removed sections.
            tx.execute(
                "UPDATE node_versions SET superseded_at=?1
                WHERE space_id=?2 AND superseded_at IS NULL
                AND json_extract(metadata,'$.imported_from')='markdown' AND source=?3",
                params![recorded, config.space, doc.path.to_str()],
            )?;
            put_node(&tx, config, &attribution, recorded, doc, None)?;
            for section in 0..doc.sections.len() {
                put_node(&tx, config, &attribution, recorded, doc, Some(section))?;
            }
            // Edges are assertions too: removed links/sections must not stay
            // current after editing a file, but their history is retained.
            // Materialized paths are a disposable current-view cache. Remove
            // cached paths through these edges, while keeping canonical history.
            tx.execute(
                "WITH owned AS (SELECT edge_id FROM edge_versions
                    WHERE space_id=?1 AND json_extract(metadata,'$.markdown_owner')=?2)
                 DELETE FROM materialized_paths WHERE space_id=?1
                    AND (ab_edge_id IN owned OR bc_edge_id IN owned)",
                params![config.space, doc.id],
            )?;
            tx.execute(
                "UPDATE edge_versions SET superseded_at=?1
                WHERE space_id=?2 AND superseded_at IS NULL
                AND json_extract(metadata,'$.markdown_owner')=?3",
                params![recorded, config.space, doc.id],
            )?;
        }
        for (from, to, relation, index) in &links {
            put_edge(
                &tx,
                config,
                &attribution,
                recorded,
                &docs[*index],
                (from, to, relation),
            )?;
        }
        tx.commit()?;
        crate::print_json(&report.json(false))
    }

    fn secret_path(path: &Path) -> bool {
        let path = path.to_string_lossy().to_ascii_lowercase();
        [".secrets", "credentials", "auth.json", ".env"]
            .iter()
            .any(|s| path.contains(s))
    }

    fn ignored_directory(path: &Path) -> bool {
        path.components().any(|c| {
            matches!(c, Component::Normal(s)
            if s.to_str().is_some_and(|s| s.starts_with('.') || s == "node_modules"))
        })
    }

    fn walk(
        path: &Path,
        root: &Path,
        excluded: &[String],
        visited: &mut HashSet<PathBuf>,
        files: &mut Vec<PathBuf>,
        report: &mut Report,
    ) {
        if secret_path(path) {
            report.skip(path, "secret_path");
            return;
        }
        let resolved = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                report.skip(path, "unreadable_or_broken_symlink");
                return;
            }
        };
        if !resolved.starts_with(root) {
            report.skip(path, "symlink_outside_root");
            return;
        }
        if secret_path(&resolved) {
            report.skip(path, "secret_path");
            return;
        }
        let Ok(meta) = fs::metadata(&resolved) else {
            report.skip(path, "unreadable");
            return;
        };
        // Examine only components below the selected root. Explicit MEMORY.md
        // under an agent's hidden home directory remains a useful single input.
        let relative = resolved.strip_prefix(root).unwrap_or(&resolved);
        let parent = if meta.is_dir() {
            relative
        } else {
            relative.parent().unwrap_or(Path::new(""))
        };
        if ignored_directory(parent)
            || (meta.is_dir()
                && path
                    .file_name()
                    .is_some_and(|s| s.to_string_lossy().starts_with('.') || s == "node_modules"))
        {
            report.skip(path, "ignored_directory");
            return;
        }
        // A file is checked by the directories above it, after symlinks are
        // resolved: a link elsewhere must not bring an excluded directory's file in,
        // while a file merely named like an excluded directory stays importable.
        let excluded_scope = if meta.is_dir() {
            relative
        } else {
            relative.parent().unwrap_or(Path::new(""))
        };
        if excluded_scope.components().any(
            |c| matches!(c, Component::Normal(s) if excluded.iter().any(|name| s == name.as_str())),
        ) {
            report.skip(path, "excluded_by_user");
            return;
        }
        if !visited.insert(resolved.clone()) {
            return;
        }
        if meta.is_dir() {
            let Ok(entries) = fs::read_dir(&resolved) else {
                report.skip(path, "unreadable_directory");
                return;
            };
            let mut children = Vec::new();
            for entry in entries {
                match entry {
                    Ok(e) => children.push(e.path()),
                    Err(_) => report.skip(path, "unreadable_entry"),
                }
            }
            children.sort();
            for child in children {
                walk(&child, root, excluded, visited, files, report);
            }
        } else if meta.is_file() && resolved.extension().is_some_and(|e| e == "md") {
            report.files_seen += 1;
            if meta.len() > MAX_BYTES {
                report.skip(path, "larger_than_1_mib");
            } else {
                files.push(resolved);
            }
        } else if !meta.is_file() {
            report.skip(path, "not_a_regular_file");
        }
    }

    // Deliberately bounded YAML subset for note metadata: quoted/plain scalars,
    // flow sequences and block sequences. Unknown fields are stripped, never
    // interpreted. Unsupported values are reported without echoing their text.
    fn scalar(value: &str) -> Option<String> {
        let value = value.trim();
        if value.starts_with('"') {
            let mut values = serde_json::Deserializer::from_str(value).into_iter::<String>();
            let parsed = values.next()?.ok()?;
            let tail = value[values.byte_offset()..].trim();
            return (tail.is_empty() || tail.starts_with('#')).then_some(parsed);
        }
        if let Some(rest) = value.strip_prefix('\'') {
            let end = rest.rfind('\'')?;
            return Some(rest[..end].replace("''", "'"));
        }
        if value.starts_with(['&', '*', '!', '{', '[', '|', '>']) {
            return None;
        }
        Some(value.split(" #").next().unwrap_or("").trim().to_owned())
    }

    fn flow_tags(value: &str) -> Option<Vec<String>> {
        let value = value.trim();
        if !value.starts_with('[') {
            return Some(vec![scalar(value)?]);
        }
        let end = value.rfind(']')?;
        let mut tags = Vec::new();
        let mut quote = None;
        let mut start = 1;
        let mut escaped = false;
        for (i, c) in value[1..end].char_indices().map(|(i, c)| (i + 1, c)) {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' && quote == Some('"') {
                escaped = true;
                continue;
            }
            if matches!(c, '\'' | '"') {
                if quote == Some(c) {
                    quote = None;
                } else if quote.is_none() {
                    quote = Some(c);
                }
            }
            if c == ',' && quote.is_none() {
                tags.push(scalar(&value[start..i])?);
                start = i + 1;
            }
        }
        if quote.is_some() {
            return None;
        }
        tags.push(scalar(&value[start..end])?);
        Some(tags)
    }

    fn front_matter<'a>(text: &'a str, path: &Path, report: &mut Report) -> (&'a str, Value) {
        let text = text.trim_start_matches('\u{feff}');
        let mut metadata = json!({});
        let mut lines = text.split_inclusive('\n');
        if lines.next().map(str::trim) != Some("---") {
            return (text, metadata);
        }
        let mut offset = text.find('\n').map_or(text.len(), |i| i + 1);
        let mut yaml = Vec::new();
        let mut closed = false;
        for line in lines {
            offset += line.len();
            if matches!(line.trim(), "---" | "...") {
                closed = true;
                break;
            }
            yaml.push(line);
        }
        if !closed {
            report.warn(path, "unclosed_front_matter; treated as body");
            return (text, metadata);
        }
        let mut block_tags = false;
        let mut tags = Vec::new();
        for line in yaml {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if block_tags && trimmed.starts_with("- ") {
                if let Some(tag) = scalar(&trimmed[2..]) {
                    tags.push(tag);
                } else {
                    report.warn(path, "unsupported_front_matter_tag");
                }
                continue;
            }
            block_tags = false;
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            match key {
                "tags" => {
                    block_tags = value.trim().is_empty();
                    if !block_tags {
                        if let Some(values) = flow_tags(value) {
                            tags.extend(values);
                        } else {
                            report.warn(path, "unsupported_front_matter_tags");
                        }
                    }
                }
                "type" | "date" | "created" => {
                    if let Some(value) = scalar(value) {
                        metadata[key] = json!(value);
                    } else {
                        report.warn(path, "unsupported_front_matter_scalar");
                    }
                }
                _ => {}
            }
        }
        metadata["tags"] = json!(tags);
        (&text[offset..], metadata)
    }

    /// Headings outside fenced code only; offsets are UTF-8 byte boundaries.
    /// Sections by `##`, else by `###`, else the whole body; the text before the first
    /// heading counts as an intro; anything still too long is cut into parts.
    fn split_sections(body: &str, label: &str) -> Vec<(String, String)> {
        let mut marks = headings(body, "## ");
        if marks.is_empty() {
            marks = headings(body, "### ");
        }
        let mut raw = Vec::new();
        let first = marks.first().map_or(body.len(), |(offset, _)| *offset);
        // The `# Title` line is the document label, not intro text.
        let intro = body[..first]
            .split_inclusive('\n')
            .filter(|line| !line.starts_with("# "))
            .collect::<String>();
        if intro.trim().len() >= INTRO_MIN_BYTES || marks.is_empty() {
            raw.push((format!("{label} (intro)"), intro));
        }
        for (i, (start, title)) in marks.iter().enumerate() {
            let end = marks.get(i + 1).map_or(body.len(), |(offset, _)| *offset);
            raw.push((title.clone(), body[*start..end].to_owned()));
        }
        let mut sections = Vec::new();
        for (title, text) in raw {
            if text.trim().is_empty() {
                continue;
            }
            if text.len() <= SECTION_MAX_BYTES {
                sections.push((title, text));
                continue;
            }
            let parts = split_parts(&text);
            let total = parts.len();
            for (n, part) in parts.into_iter().enumerate() {
                sections.push((format!("{title} (part {}/{total})", n + 1), part));
            }
        }
        sections
    }

    /// Cuts text into parts of at most PART_BYTES, preferring a blank line, then any
    /// line end, and only inside a line that alone is longer than a part.
    fn split_parts(text: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut rest = text;
        while rest.len() > PART_BYTES {
            let window = &rest[..floor_char_boundary(rest, PART_BYTES)];
            let cut = window
                .rfind("\n\n")
                .map(|i| i + 2)
                .filter(|i| *i >= PART_BYTES / 2)
                .or_else(|| {
                    window
                        .rfind('\n')
                        .map(|i| i + 1)
                        .filter(|i| *i >= PART_BYTES / 4)
                })
                .unwrap_or(window.len());
            parts.push(rest[..cut].to_owned());
            rest = &rest[cut..];
        }
        if !rest.trim().is_empty() {
            parts.push(rest.to_owned());
        }
        parts
    }

    fn floor_char_boundary(text: &str, index: usize) -> usize {
        let mut index = index.min(text.len());
        while !text.is_char_boundary(index) {
            index -= 1;
        }
        index
    }

    fn headings(body: &str, prefix: &str) -> Vec<(usize, String)> {
        let mut result = Vec::new();
        let mut fence: Option<(char, usize)> = None;
        let mut offset = 0;
        for line in body.split_inclusive('\n') {
            let trimmed = line.trim_start();
            let marker = trimmed.chars().next().unwrap_or(' ');
            let count = trimmed.chars().take_while(|c| *c == marker).count();
            if matches!(marker, '`' | '~') && count >= 3 {
                match fence {
                    None => fence = Some((marker, count)),
                    Some((c, n))
                        if c == marker && count >= n && trimmed[count..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
            } else if fence.is_none()
                && let Some(title) = line.strip_prefix(prefix)
            {
                result.push((offset, title.trim().trim_end_matches('#').trim().to_owned()));
            }
            offset += line.len();
        }
        result
    }

    /// Accept absolute ISO dates/times or Unix seconds, never SQLite's `now`
    /// shorthand or an impossible calendar date silently normalized by SQLite.
    fn event_time(clock: &Connection, value: &str) -> Result<Option<i64>, Box<dyn Error>> {
        if let Ok(epoch) = value.parse::<i64>() {
            return Ok(Some(epoch));
        }
        let bytes = value.as_bytes();
        if bytes.len() < 10
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || ![&bytes[..4], &bytes[5..7], &bytes[8..10]]
                .iter()
                .all(|p| p.iter().all(u8::is_ascii_digit))
        {
            return Ok(None);
        }
        let year = value[..4].parse::<u32>()?;
        let month = value[5..7].parse::<usize>()?;
        let day = value[8..10].parse::<u32>()?;
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let days = [
            0,
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ];
        if month == 0 || month > 12 || day == 0 || day > days[month] {
            return Ok(None);
        }
        Ok(clock.query_row("SELECT unixepoch(?1)", [value], |r| r.get(0))?)
    }

    fn read_document(
        path: &Path,
        root: &Path,
        extra_tags: &[String],
        clock: &Connection,
        report: &mut Report,
    ) -> Result<Option<Document>, Box<dyn Error>> {
        if path.to_str().is_none() {
            report.skip(path, "non_utf8_source_path");
            return Ok(None);
        }
        // NOFOLLOW closes the final-component symlink race; the size is checked
        // on the open descriptor and the read itself is also capped.
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        use std::os::fd::AsRawFd;
        let opened = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if !opened.starts_with(root) || opened != path || secret_path(&opened) {
            report.skip(path, "source_changed_during_scan");
            return Ok(None);
        }
        let meta = file.metadata()?;
        if !meta.is_file() {
            report.skip(path, "not_a_regular_file");
            return Ok(None);
        }
        let mut raw = Vec::new();
        file.take(MAX_BYTES + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_BYTES {
            report.skip(path, "larger_than_1_mib");
            return Ok(None);
        }
        let hash = digest(&raw);
        let (safe, redacted) = crate::redact::mask_secrets(std::str::from_utf8(&raw)?);
        if redacted {
            report.warn(path, "secret_lines_redacted");
        }
        let (body, mut metadata) = front_matter(&safe, path, report);
        let mut tags: Vec<String> = metadata["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        tags.extend(extra_tags.iter().map(|t| crate::redact::mask_secrets(t).0));
        tags.retain(|t| !t.is_empty());
        tags.sort();
        tags.dedup();
        metadata["tags"] = json!(tags);
        metadata["import_tags"] = json!(
            extra_tags
                .iter()
                .map(|t| crate::redact::mask_secrets(t).0)
                .collect::<Vec<_>>()
        );
        let modified = meta.modified()?;
        let mtime = match modified.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_secs())?,
            Err(e) => {
                -i64::try_from(e.duration().as_secs())?
                    - i64::from(e.duration().subsec_nanos() != 0)
            }
        };
        let mut time = None;
        for field in ["date", "created"] {
            if let Some(value) = metadata[field].as_str() {
                let parsed = event_time(clock, value)?;
                if let Some(parsed) = parsed {
                    time = Some(parsed);
                    break;
                }
                report.warn(path, "invalid_front_matter_date; trying created or mtime");
            }
        }
        metadata["imported_from"] = json!("markdown");
        metadata["content_hash"] = json!(hash);
        metadata["untrusted"] = json!(true);
        let label = headings(body, "# ")
            .into_iter()
            .next()
            .map(|(_, s)| s)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        metadata["sectioning"] = json!(SECTIONING);
        let sections = if body.len() > SECTION_BYTES {
            split_sections(body, &label)
        } else {
            Vec::new()
        };
        Ok(Some(Document {
            path: path.to_owned(),
            id: format!("md:{}", digest(path.as_os_str().as_encoded_bytes())),
            hash,
            label,
            body: body.to_owned(),
            metadata,
            time: time.unwrap_or(mtime),
            sections,
        }))
    }

    fn plan(
        db: Option<&Connection>,
        space: &str,
        docs: &mut [Document],
        report: &mut Report,
    ) -> Result<Vec<usize>, Box<dyn Error>> {
        let mut changed = Vec::new();
        for (i, doc) in docs.iter_mut().enumerate() {
            let previous: Option<(String, String)> = match db {
                Some(db) => db.query_row("SELECT source,metadata FROM node_versions WHERE space_id=?1 AND node_id=?2 AND superseded_at IS NULL ORDER BY revision DESC LIMIT 1",
                    params![space, doc.id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?,
                None => None,
            };
            if let Some((source, metadata)) = previous {
                if Path::new(&source) != doc.path {
                    return Err("Markdown source identity collision".into());
                }
                let metadata: Value = serde_json::from_str(&metadata)?;
                let mut imported_tags = Vec::new();
                for value in [&metadata["import_tags"], &doc.metadata["import_tags"]] {
                    imported_tags.extend(
                        value
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
                // First-seen order, not sorted: `brain stats` groups a source by its
                // first import tag, which must not move when the file is re-imported.
                let mut seen = HashSet::new();
                imported_tags.retain(|tag| seen.insert(tag.clone()));
                let mut tags: Vec<String> = doc.metadata["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                tags.extend(imported_tags.iter().cloned());
                tags.sort();
                tags.dedup();
                doc.metadata["tags"] = json!(tags);
                doc.metadata["import_tags"] = json!(imported_tags);
                // import_tags too: a tag already present in front matter leaves `tags`
                // unchanged, yet it is a new import tag that must be stored.
                if metadata["content_hash"] == doc.hash
                    && metadata["sectioning"] == doc.metadata["sectioning"]
                    && metadata["tags"] == doc.metadata["tags"]
                    && metadata["import_tags"] == doc.metadata["import_tags"]
                {
                    report.unchanged += 1;
                    continue;
                }
                report.updated += 1;
            } else {
                report.imported += 1;
            }
            changed.push(i);
        }
        Ok(changed)
    }

    fn report_missing(
        db: &Connection,
        space: &str,
        source: &Path,
        directory: bool,
        report: &mut Report,
    ) -> Result<(), Box<dyn Error>> {
        let mut stmt = db.prepare("SELECT DISTINCT source FROM node_versions WHERE space_id=?1 AND superseded_at IS NULL AND json_extract(metadata,'$.imported_from')='markdown' AND json_extract(metadata,'$.parent_id') IS NULL")?;
        for path in stmt.query_map([space], |r| r.get::<_, String>(0))? {
            let path = PathBuf::from(path?);
            if (path == source || (directory && path.starts_with(source)))
                && matches!(fs::metadata(&path), Err(e) if e.kind()==std::io::ErrorKind::NotFound)
            {
                report.warn(&path, "deleted_source; stored history was retained");
            }
        }
        Ok(())
    }

    // (source ID, target ID, relation, owning document index).
    type Link = (String, String, &'static str, usize);
    fn section_id(doc: &Document, index: usize) -> String {
        format!("{}:section:{index}", doc.id)
    }
    fn edge_id(from: &str, to: &str, relation: &str) -> String {
        format!(
            "md-edge:{}",
            digest(format!("{from}\0{relation}\0{to}").as_bytes())
        )
    }

    fn targets(body: &str) -> Vec<(String, bool)> {
        let mut targets = Vec::new();
        // Remove code blocks and inline code before interpreting links.
        let mut fence = None;
        for line in body.lines() {
            let trimmed = line.trim_start();
            let marker = trimmed.chars().next().unwrap_or(' ');
            let count = trimmed.chars().take_while(|c| *c == marker).count();
            if matches!(marker, '`' | '~') && count >= 3 {
                match fence {
                    None => fence = Some((marker, count)),
                    Some((c, n))
                        if c == marker && count >= n && trimmed[count..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
                continue;
            }
            if fence.is_some() {
                continue;
            }
            for prose in line.split('`').step_by(2) {
                let mut rest = prose;
                while let Some(start) = rest.find("[[") {
                    rest = &rest[start + 2..];
                    let Some(end) = rest.find("]]") else {
                        break;
                    };
                    targets.push((rest[..end].split('|').next().unwrap_or("").to_owned(), true));
                    rest = &rest[end + 2..];
                }
                rest = prose;
                while let Some(start) = rest.find("](") {
                    rest = &rest[start + 2..];
                    let Some(end) = rest.find(')') else {
                        break;
                    };
                    let target = rest[..end].trim();
                    let target = if let Some(target) = target.strip_prefix('<') {
                        target.split('>').next().unwrap_or("")
                    } else {
                        target.split_whitespace().next().unwrap_or("")
                    };
                    targets.push((target.to_owned(), false));
                    rest = &rest[end + 1..];
                }
            }
        }
        targets
    }

    fn decode_path(text: &str) -> Option<String> {
        let mut out = Vec::new();
        let mut bytes = text.bytes();
        while let Some(b) = bytes.next() {
            if b == b'%' {
                let hi = char::from(bytes.next()?).to_digit(16)?;
                let lo = char::from(bytes.next()?).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
            } else {
                out.push(b);
            }
        }
        String::from_utf8(out).ok()
    }

    fn links(docs: &[Document], root: &Path, report: &mut Report) -> Vec<Link> {
        let by_path: HashMap<_, _> = docs
            .iter()
            .map(|d| (d.path.clone(), d.id.clone()))
            .collect();
        let mut by_stem: HashMap<String, Vec<&Document>> = HashMap::new();
        for doc in docs {
            by_stem
                .entry(
                    doc.path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )
                .or_default()
                .push(doc);
        }
        let mut links = Vec::new();
        let mut seen = HashSet::new();
        for (index, doc) in docs.iter().enumerate() {
            for section in 0..doc.sections.len() {
                links.push((doc.id.clone(), section_id(doc, section), "contains", index));
            }
            for (target, wiki) in targets(&doc.body) {
                let target = target.split('#').next().unwrap_or("");
                let Some(mut target) = decode_path(target) else {
                    report.warn(&doc.path, "invalid_link_encoding");
                    continue;
                };
                if target.is_empty() || target.contains(':') || Path::new(&target).is_absolute() {
                    continue;
                }
                if wiki && !target.ends_with(".md") {
                    target.push_str(".md");
                }
                if !target.ends_with(".md") {
                    continue;
                }
                let mut resolved = None;
                for base in [doc.path.parent().unwrap_or(root), root] {
                    if let Ok(path) = base.join(&target).canonicalize()
                        && let Some(id) = by_path.get(&path)
                    {
                        resolved = Some(id.clone());
                        break;
                    }
                    if !wiki {
                        break;
                    }
                }
                if resolved.is_none()
                    && wiki
                    && !target.contains('/')
                    && let Some(matches) = by_stem.get(target.trim_end_matches(".md"))
                    && matches.len() == 1
                {
                    resolved = Some(matches[0].id.clone());
                }
                if let Some(to) = resolved {
                    if seen.insert((doc.id.clone(), to.clone())) {
                        links.push((doc.id.clone(), to, "links_to", index));
                    }
                } else {
                    report.warn(&doc.path, "unresolved_or_ambiguous_local_link");
                }
            }
        }
        links
    }

    fn edge_current(
        db: Option<&Connection>,
        space: &str,
        id: &str,
    ) -> Result<bool, Box<dyn Error>> {
        Ok(match db { Some(db) => db.query_row("SELECT EXISTS(SELECT 1 FROM edge_versions WHERE space_id=?1 AND edge_id=?2 AND superseded_at IS NULL)", params![space,id], |r| r.get(0))?, None => false })
    }

    fn put_node(
        db: &Connection,
        config: &crate::RuntimeConfig,
        attr: &Attribution,
        recorded: i64,
        doc: &Document,
        section: Option<usize>,
    ) -> Result<(), Box<dyn Error>> {
        let id = section.map_or_else(|| doc.id.clone(), |i| section_id(doc, i));
        let (label, body) = section.map_or((&doc.label, &doc.body), |i| {
            (&doc.sections[i].0, &doc.sections[i].1)
        });
        let mut metadata = doc.metadata.clone();
        if let Some(i) = section {
            metadata["parent_id"] = json!(doc.id);
            metadata["section"] = json!(i);
        }
        db.execute("INSERT INTO nodes(id,space_id,created_by_agent_id,created_in_session_id,created_at,last_accessed,salience,source_weight,salience_layer,token_len)
            VALUES(?1,?2,?3,?4,?5,?5,0.9,0.9,'hot',?6) ON CONFLICT(space_id,id) DO UPDATE SET token_len=excluded.token_len,last_accessed=excluded.last_accessed,salience=0.9,salience_layer='hot',next_salience_at=0",
            params![id,config.space,attr.agent_id,attr.session_id,recorded,(label.len()+body.len()+1) as i64])?;
        db.execute("INSERT INTO node_versions(node_id,space_id,revision,kind,label,body,source,metadata,valid_from,recorded_at,agent_id,session_id)
            VALUES(?1,?2,(SELECT coalesce(max(revision),0)+1 FROM node_versions WHERE node_id=?1 AND space_id=?2),'document',?3,?4,?5,?6,?7,?8,?9,?10)",
            params![id,config.space,label,body,doc.path.to_str(),metadata.to_string(),doc.time,recorded,attr.agent_id,attr.session_id])?;
        Ok(())
    }

    fn put_edge(
        db: &Connection,
        config: &crate::RuntimeConfig,
        attr: &Attribution,
        recorded: i64,
        doc: &Document,
        link: (&str, &str, &str),
    ) -> Result<(), Box<dyn Error>> {
        let (from, to, relation) = link;
        let id = edge_id(from, to, relation);
        if edge_current(Some(db), &config.space, &id)? {
            return Ok(());
        }
        let inserted = db.execute("INSERT INTO edges(id,space_id,from_node_id,to_node_id,created_by_agent_id,created_in_session_id,created_at,last_accessed,salience)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?7,0.6) ON CONFLICT(space_id,id) DO NOTHING", params![id,config.space,from,to,attr.agent_id,attr.session_id,recorded])?;
        if inserted > 0 {
            db.execute("UPDATE nodes SET degree=degree+1,next_salience_at=0 WHERE space_id=?1 AND id IN (?2,?3)", params![config.space,from,to])?;
        }
        db.execute("INSERT INTO edge_versions(edge_id,space_id,revision,relation,weight,confidence,metadata,valid_from,recorded_at,agent_id,session_id)
            VALUES(?1,?2,(SELECT coalesce(max(revision),0)+1 FROM edge_versions WHERE edge_id=?1 AND space_id=?2),?3,0.7,1.0,?4,?5,?6,?7,?8)",
            params![id,config.space,relation,json!({"imported_from":"markdown","markdown_owner":doc.id}).to_string(),doc.time,recorded,attr.agent_id,attr.session_id])?;
        db.execute("INSERT INTO path_update_queue(space_id,edge_id,queued_at) VALUES(?1,?2,?3) ON CONFLICT(space_id,edge_id) DO UPDATE SET queued_at=excluded.queued_at", params![config.space,id,recorded])?;
        Ok(())
    }

    /// SHA-256, kept local because this slice may not change Cargo dependencies.
    /// Stable across processes/releases (unlike DefaultHasher). Raw source bytes
    /// are hashed but only redacted text is persisted. Verified with known vectors.
    fn digest(bytes: &[u8]) -> String {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut data = bytes.to_vec();
        data.push(0x80);
        while data.len() % 64 != 56 {
            data.push(0);
        }
        data.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
        for chunk in data.chunks_exact(64) {
            let mut w = [0u32; 64];
            for (i, word) in chunk.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes(word.try_into().expect("four byte word"));
            }
            for i in 16..64 {
                let a = w[i - 15];
                let b = w[i - 2];
                w[i] = w[i - 16]
                    .wrapping_add(a.rotate_right(7) ^ a.rotate_right(18) ^ (a >> 3))
                    .wrapping_add(w[i - 7])
                    .wrapping_add(b.rotate_right(17) ^ b.rotate_right(19) ^ (b >> 10));
            }
            let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
            for i in 0..64 {
                let t1 = hh
                    .wrapping_add(e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25))
                    .wrapping_add((e & f) ^ (!e & g))
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let t2 = (a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22))
                    .wrapping_add((a & b) ^ (a & c) ^ (b & c));
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
                *slot = slot.wrapping_add(value);
            }
        }
        h.iter().map(|v| format!("{v:08x}")).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn sha256_known_vectors() {
            assert_eq!(
                digest(b""),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            );
            assert_eq!(
                digest(b"abc"),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            );
            assert_eq!(
                digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
            );
            assert_eq!(
                digest(&vec![b'a'; 1_000_000]),
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
            );
        }
    }
}
