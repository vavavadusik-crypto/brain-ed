//! Explicit graph import with provenance and bounded validation.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::Path;

use brain_core::{Attribution, NewEdge, NewNode, NodeKind, SqliteStorage, Storage, Timestamp};
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
    #[arg(long, default_value = "graphify", value_parser = ["graphify", "hermes"])]
    from: String,

    /// Report what would change and write nothing.
    #[arg(long)]
    dry_run: bool,
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
        let label = fields
            .get(1)
            .and_then(Value::as_str)
            .unwrap_or(source_id)
            .to_owned();
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

fn write_nodes(
    storage: &SqliteStorage,
    space: &str,
    incoming: &[(String, String, NodeKind)],
    stored: &HashMap<String, String>,
    now: Timestamp,
    attribution: &Attribution,
) -> Result<usize, Box<dyn Error>> {
    let batch = incoming
        .iter()
        .filter(|(id, label, _)| stored.get(id) != Some(label))
        .map(|(id, label, kind)| NewNode {
            id: id.clone(),
            space_id: space.to_owned(),
            kind: *kind,
            label: label.clone(),
            body: label.clone(),
            source: Some("graphify".to_owned()),
            metadata_json: json!({"imported_from": "graphify"}).to_string(),
            valid_from: now,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 0.6,
            source_weight: 0.9,
        })
        .collect::<Vec<_>>();
    if batch.is_empty() {
        return Ok(0);
    }
    storage.insert_nodes(&batch)?;
    Ok(batch.len())
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

    fn scratch(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brain-import-{label}-{}-{}",
            std::process::id(),
            crate::now_unix_seconds()
        ));
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
