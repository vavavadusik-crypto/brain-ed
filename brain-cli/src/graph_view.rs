//! The graph, for looking at.
//!
//! The settings window could say the memory held 10,590 nodes and 31,131 edges
//! and show nothing of them. Numbers do not tell you whether the thing knows
//! what you think it knows — a picture does.
//!
//! The viewer itself is the one the owner already had and already trusted:
//! rewriting a working 3D renderer to draw the same graph would have been work
//! spent reaching a place they were standing in. This module's job is only to
//! feed it, in the `brain-data-v2` shape it expects.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

/// Nodes sent when the caller does not say.
///
/// Chosen for the picture rather than the count: the most connected nodes
/// mostly link to neighbours further down the list, so a small selection keeps
/// few edges and looks like scattered dust rather than a structure.
pub(crate) const DEFAULT_LIMIT: usize = 4_000;

/// Ceiling regardless of what the caller asks for.
const MAX_LIMIT: usize = 20_000;

/// Field order the viewer reads positionally. Changing it silently misaligns
/// every column, so it is written once and used to build both the schema and
/// the rows.
const NODE_FIELDS: [&str; 18] = [
    "id",
    "label",
    "type",
    "community",
    "source",
    "glyph",
    "color",
    "lobe",
    "importance",
    "visual_size",
    "activity_score",
    "activity_ts",
    "x",
    "y",
    "z",
    "phase",
    "degree",
    "first_seen_ts",
];

const LINK_FIELDS: [&str; 4] = ["source_index", "target_index", "relation", "confidence"];

/// Builds the graph document in the shape the viewer loads.
///
/// # Errors
///
/// Returns the SQLite failure when the database cannot be read.
pub(crate) fn document(database: &Path, space: &str, limit: usize) -> rusqlite::Result<Value> {
    selected_document(database, space, limit, None)
}

pub(crate) fn selected_document(
    database: &Path,
    space: &str,
    limit: usize,
    ids: Option<&[String]>,
) -> rusqlite::Result<Value> {
    let limit = limit.clamp(1, MAX_LIMIT) as i64;
    let now = crate::now_unix_seconds();
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    // Degree first, salience second: a node nobody links to is not part of the
    // shape of the graph however important it is on its own.
    let mut statement = connection.prepare(
        r#"SELECT n.id, v.label, v.kind, n.degree, n.salience, n.salience_layer,
                  n.created_at, n.last_accessed
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.recorded_at <= ?3 AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
            AND v.valid_from <= ?3 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
           WHERE n.space_id = ?1
           AND (?4 IS NULL OR n.id IN (SELECT value FROM json_each(?4)))
           ORDER BY n.degree DESC, n.salience DESC, n.id
           LIMIT ?2"#,
    )?;

    struct Row {
        id: String,
        label: String,
        kind: String,
        degree: i64,
        layer: String,
        created_at: i64,
        last_accessed: i64,
    }

    let rows = statement
        .query_map(
            params![
                space,
                limit,
                now,
                ids.map(|ids| serde_json::to_string(ids).expect("ids"))
            ],
            |row| {
                Ok(Row {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    kind: row.get(2)?,
                    degree: row.get(3)?,
                    // Column 4 (salience) only orders the selection.
                    layer: row.get(5)?,
                    created_at: row.get(6)?,
                    last_accessed: row.get(7)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let index: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(position, row)| (row.id.as_str(), position))
        .collect();

    let max_degree = rows.iter().map(|row| row.degree).max().unwrap_or(1).max(1) as f64;
    let nodes = rows
        .iter()
        .enumerate()
        .map(|(position, row)| {
            let (community, lobe) = lobe_for(&row.kind);
            let importance = (row.degree as f64 / max_degree).clamp(0.05, 1.0);
            let (x, y, z) = seed_position(position, rows.len());
            json!([
                row.id,
                shorten(&row.label),
                kind_code(&row.kind),
                community,
                0,
                glyph_for(&row.kind),
                colour_for(&row.kind),
                lobe,
                round3(importance),
                round3(4.0 + importance * 14.0),
                round3(match row.layer.as_str() {
                    "hot" => 1.0,
                    "warm" => 0.5,
                    _ => 0.15,
                }),
                row.last_accessed,
                round3(x),
                round3(y),
                round3(z),
                round3((position % 360) as f64 / 360.0),
                row.degree,
                row.created_at,
            ])
        })
        .collect::<Vec<_>>();

    let mut lobe_counts: HashMap<usize, usize> = HashMap::new();
    for row in &rows {
        *lobe_counts.entry(kind_index(&row.kind)).or_default() += 1;
    }

    let links = links_among(&connection, space, &index, now)?;

    let total_nodes: i64 = connection.query_row(
        "SELECT count(*) FROM node_versions AS v
         JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
         WHERE v.space_id = ?1
           AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)",
        params![space, now],
        |row| row.get(0),
    )?;
    let total_edges: i64 = connection.query_row(
        "SELECT count(*) FROM edge_versions AS v
         JOIN edges AS e ON e.id = v.edge_id AND e.space_id = v.space_id
         WHERE v.space_id = ?1
           AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)",
        params![space, now],
        |row| row.get(0),
    )?;

    Ok(json!({
        "format": "brain-data-v2",
        "schema": { "node": NODE_FIELDS, "link": LINK_FIELDS },
        // The rows carry indexes, not names; without these the viewer reads
        // past the end of an empty array and stops before drawing anything.
        "dictionaries": {
            "types": KIND_NAMES,
            "type_colors": KIND_COLOURS,
            "colors": KIND_COLOURS,
            "sources": [""],
            "relations": ["calls", "mentions", "relates_to", "contains", "other"],
            "confidences": ["EXTRACTED", "INFERRED"],
        },
        "nodes": nodes,
        "links": links,
        "hyperedges": [],
        "lobes": lobes_document(&lobe_counts),
        "stats": {
            "nodes": rows.len(),
            "links": links.len(),
            "hyperedges": 0,
            "communities": lobe_counts.len(),
            // "Active" here means the hot salience layer: what the memory is
            // currently working with, as opposed to what it merely still holds.
            "active_nodes": rows.iter().filter(|row| row.layer == "hot").count(),
            "activity_min_ts": rows.iter().map(|row| row.last_accessed).min().unwrap_or(0),
            "activity_max_ts": rows.iter().map(|row| row.last_accessed).max().unwrap_or(0),
            "first_seen_min_ts": rows.iter().map(|row| row.created_at).min().unwrap_or(0),
            "first_seen_max_ts": rows.iter().map(|row| row.created_at).max().unwrap_or(0),
            "ancient_nodes": rows.iter().filter(|row| row.layer == "cold").count(),
            "now_ts": rows.iter().map(|row| row.last_accessed).max().unwrap_or(0),
            "total_nodes": total_nodes,
            "total_edges": total_edges,
            "space": space,
            // Said outright: a picture of part of the memory is a sample, and a
            // viewer who believes they are seeing all of it is being misled.
            "note": format!(
                "показаны {} самых связанных узлов из {total_nodes}; связи — только между ними",
                rows.len()
            ),
        },
        "vault": {},
        "ops": {},
    }))
}

/// Links whose both ends are in the visible set, as index pairs.
fn links_among(
    connection: &Connection,
    space: &str,
    index: &HashMap<&str, usize>,
    now: i64,
) -> rusqlite::Result<Vec<Value>> {
    if index.is_empty() {
        return Ok(Vec::new());
    }
    // Every edge is read and filtered in memory rather than sent as a bound
    // list: SQLite takes 999 parameters by default, and the visible set is
    // thousands of nodes.
    let mut statement = connection.prepare(
        r#"SELECT e.from_node_id, e.to_node_id, v.relation, v.confidence
           FROM edges AS e
           JOIN edge_versions AS v
             ON v.space_id = e.space_id AND v.edge_id = e.id
            AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)
            AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           WHERE e.space_id = ?1"#,
    )?;
    let rows = statement.query_map(params![space, now], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;

    let mut links = Vec::new();
    for row in rows {
        let (from, to, relation, confidence) = row?;
        let (Some(&source), Some(&target)) = (index.get(from.as_str()), index.get(to.as_str()))
        else {
            continue;
        };
        // Confidence is a dictionary index too: 0 extracted, 1 inferred.
        let confidence_code = usize::from(confidence < 0.75);
        links.push(json!([
            source,
            target,
            relation_code(&relation),
            confidence_code
        ]));
    }
    Ok(links)
}

/// Kind names in the order the viewer's dictionary lists them.
///
/// The node row stores an index into this array, not a name, so the order here
/// and the order sent as `dictionaries.types` have to be the same array — which
/// is why it is one constant used for both.
const KIND_NAMES: [&str; 8] = [
    "code", "concept", "decision", "document", "error", "memory", "entity", "session",
];

const KIND_COLOURS: [&str; 8] = [
    "#e0af68", "#9ece6a", "#4ade9f", "#bb9af7", "#f0715f", "#7aa2f7", "#ff9e64", "#7dcfff",
];

/// Index of a kind in the shared dictionary.
fn kind_index(kind: &str) -> usize {
    KIND_NAMES
        .iter()
        .position(|name| *name == kind)
        .unwrap_or(6)
}

/// Which lobe a record belongs to, and its community number.
///
/// Grouping by kind is what makes the picture readable: decisions cluster
/// apart from code, and errors apart from both.
fn lobe_for(kind: &str) -> (usize, usize) {
    let lobe = kind_index(kind);
    (lobe, lobe)
}

/// The regions the viewer arranges nodes into.
///
/// Each is a labelled ellipsoid with a centre and radii, so kinds occupy
/// separate volumes instead of one undifferentiated ball.
fn lobes_document(counts: &HashMap<usize, usize>) -> Value {
    const NAMES: [(&str, &str, &str); 8] = [
        ("code", "Код", "функции, типы, вызовы"),
        ("concept", "Понятия", "термины и идеи"),
        ("decision", "Решения", "что и почему выбрали"),
        ("document", "Документы", "заметки и файлы"),
        ("error", "Ошибки", "грабли и сбои"),
        ("memory", "Память", "сохранённое агентами"),
        ("entity", "Сущности", "имена и объекты"),
        ("session", "Сессии", "ход работы"),
    ];
    const CENTRES: [[i32; 3]; 8] = [
        [-90, 40, 30],
        [70, 50, -20],
        [0, 70, 60],
        [95, -20, 25],
        [-60, -60, -40],
        [10, -30, 70],
        [-30, 10, -80],
        [60, -70, 10],
    ];
    NAMES
        .iter()
        .enumerate()
        .map(|(index, (id, name, role))| {
            let rgb = hex_to_rgb(KIND_COLOURS[index]);
            json!({
                "id": id,
                "name": name,
                "role": role,
                "color": KIND_COLOURS[index],
                "rgb": rgb,
                "center": CENTRES[index],
                "radii": [70, 46, 58],
                "count": counts.get(&index).copied().unwrap_or(0),
                "avg_importance": 0.3,
            })
        })
        .collect::<Vec<_>>()
        .into()
}

/// Splits a `#rrggbb` string into the fractions the viewer shades with.
fn hex_to_rgb(hex: &str) -> [f64; 3] {
    let value = hex.trim_start_matches('#');
    let channel = |start: usize| {
        u8::from_str_radix(&value[start..start + 2], 16).unwrap_or(150) as f64 / 255.0
    };
    if value.len() < 6 {
        return [0.6, 0.6, 0.6];
    }
    [channel(0), channel(2), channel(4)]
}

fn kind_code(kind: &str) -> usize {
    kind_index(kind)
}

fn glyph_for(kind: &str) -> &'static str {
    match kind {
        "decision" => "DEC",
        "error" => "ERR",
        "memory" => "MEM",
        "code" => "FN",
        "document" => "DOC",
        "session" => "SES",
        "concept" => "CON",
        _ => "ENT",
    }
}

/// Index into the `colors` dictionary, which is what the row stores.
///
/// One entry per kind: the viewer looks the value up rather than reading it,
/// so a hue or a hex string here indexes past the end of the array and the
/// whole render stops.
fn colour_for(kind: &str) -> usize {
    kind_index(kind)
}

fn relation_code(relation: &str) -> usize {
    match relation {
        "calls" => 0,
        "mentions" => 1,
        "relates_to" => 2,
        "contains" => 3,
        _ => 4,
    }
}

/// A starting point on a sphere, spread by the golden angle.
///
/// The layout settles under the viewer's own physics; this only has to avoid
/// starting every node at the origin, and to be the same on every open so a
/// region stays recognisable.
fn seed_position(position: usize, total: usize) -> (f64, f64, f64) {
    let total = total.max(1) as f64;
    let index = position as f64;
    let phi = (1.0 - 2.0 * (index + 0.5) / total).clamp(-1.0, 1.0).acos();
    let theta = std::f64::consts::PI * (1.0 + 5.0_f64.sqrt()) * (index + 0.5);
    let radius = 40.0 + index.sqrt() * 1.6;
    (
        phi.sin() * theta.cos() * radius,
        phi.sin() * theta.sin() * radius,
        phi.cos() * radius,
    )
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Trims a label to something that fits beside a dot.
fn shorten(label: &str) -> String {
    const MAX_CHARS: usize = 64;
    let trimmed = label.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_owned();
    }
    let kept = trimmed.chars().take(MAX_CHARS - 1).collect::<String>();
    format!("{kept}…")
}

/// The per-node detail rows the ported viewer lazy-loads on first click
/// (`brain-details.json`, plan task T3). Each row carries its node id, so
/// reordering between requests cannot attach another node's details.
pub(crate) fn details_document(
    database: &Path,
    space: &str,
    limit: usize,
) -> rusqlite::Result<Value> {
    let limit = limit.clamp(1, MAX_LIMIT) as i64;
    let now = crate::now_unix_seconds();
    let connection = open_read_only(database)?;
    let mut statement = connection.prepare(
        r#"SELECT n.last_accessed, n.degree, v.source, n.id
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.recorded_at <= ?3 AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
            AND v.valid_from <= ?3 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
           WHERE n.space_id = ?1
           ORDER BY n.degree DESC, n.salience DESC, n.id
           LIMIT ?2"#,
    )?;
    let rows = statement
        .query_map(params![space, limit, now], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The same data-derived clock document() uses, so both documents agree.
    let now = rows.iter().map(|row| row.0).max().unwrap_or(0);
    let nodes = rows
        .iter()
        .map(|(last_accessed, degree, source, id)| {
            json!([
                source.clone(),                           // source_location
                Value::Null,                              // source_url (no editor scheme here)
                Value::Null,                              // obsidian_file
                Value::Null,                              // obsidian_url
                crate::stats::utc_minute(*last_accessed), // last_touch
                ((now - last_accessed).max(0) as f64 / 86_400.0 * 100.0).round() / 100.0,
                false,  // dirty
                0,      // hyperedge_count
                degree, // source_entity_count (its degree)
                id,     // stable join key, independent of selection order
            ])
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "format": "brain-details-v1",
        "schema": ["source_location", "source_url", "obsidian_file", "obsidian_url",
                   "last_touch", "age_days", "dirty", "hyperedge_count", "source_entity_count", "id"],
        "nodes": nodes,
    }))
}

/// One stored node as the viewer's external card reads it (`/api/node`, plan
/// task T4). None when the space has no current version of that id.
pub(crate) fn node_document(
    database: &Path,
    space: &str,
    id: &str,
) -> rusqlite::Result<Option<Value>> {
    let connection = open_read_only(database)?;
    let row = connection
        .query_row(
            r#"SELECT v.label, v.kind, v.body, v.agent_id, v.recorded_at, v.source, v.metadata
               FROM node_versions AS v
               WHERE v.node_id = ?1 AND v.space_id = ?2
                 AND v.valid_from <= ?3 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
                 AND v.recorded_at <= ?3 AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)"#,
            params![id, space, crate::now_unix_seconds()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((label, kind, body, agent, recorded_at, source, metadata)) = row else {
        return Ok(None);
    };
    let tags: Vec<String> = metadata
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|document| {
            document.get("tags")?.as_array().map(|tags| {
                tags.iter()
                    .filter_map(|tag| tag.as_str().map(str::to_owned))
                    .collect()
            })
        })
        .unwrap_or_default();
    Ok(Some(json!({
        "id": id,
        "label": label,
        "type": kind,
        "memory_type": kind,
        "text": body,
        "agent": agent,
        "updated_at": recorded_at,
        "tags": tags,
        "source_file": source,
    })))
}

/// Ranked ids for the viewer's search box (`/api/search`, plan task T5): the
/// same hybrid search `brain ask` runs, without the ask document around it.
pub(crate) fn search_document(
    database: &Path,
    space: &str,
    query: &str,
    limit: usize,
) -> Result<Value, String> {
    let storage =
        brain_core::SqliteStorage::open_read_only(database).map_err(|error| error.to_string())?;
    let search = brain_core::HybridSearch::without_vectors(&storage, "");
    let now = crate::now_unix_seconds();
    let request = brain_core::SearchQuery::new(space, query, brain_core::AsOf::new(now, now))
        .with_layers(brain_core::SearchLayers::ALL)
        .without_access_tracking()
        .with_prefix_matching();
    let quality = brain_core::profile::Profile::Balanced.settings();
    let output = search
        .search_with(&request, &quality)
        .map_err(|error| error.to_string())?;
    let results = output
        .hits
        .iter()
        .take(limit.min(10))
        .map(|hit| {
            json!({
                "id": hit.node.id,
                "label": hit.node.label,
                "score": hit.score,
                "type": hit.node.kind.to_string(),
                "path": hit.node.source,
                "lobe": hit.node.kind.to_string(),
                "community": 0,
                "importance": 0,
                "activity": 0,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!(results))
}

/// Ceiling on the inter-cluster links the document lists: the strongest
/// corridors first, the rest only acknowledged by `links_truncated`.
const MAX_CLUSTER_LINKS: usize = 4_096;

/// Rows one cluster expansion returns. The panel lists the strongest members;
/// a community of thirty thousand ids would not fit a click anyway.
const CLUSTER_DETAIL_ROWS: usize = 500;

/// Label propagation stops after this many passes even when labels still
/// flicker: the answer stays deterministic, just not necessarily converged.
const MAX_PROPAGATION_PASSES: usize = 12;

/// One node of the clustered graph, read once for the whole space.
struct GraphNode {
    id: String,
    label: String,
    kind: String,
    degree: i64,
}

/// One member of a community, the way the cluster panel lists it.
pub(crate) struct ClusterMember {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) kind: String,
    pub(crate) degree: i64,
}

/// One community of the whole space.
pub(crate) struct Cluster {
    /// How many live nodes the community holds.
    pub(crate) size: usize,
    /// The most connected member's label: what the sphere is called.
    pub(crate) label: String,
    /// The most connected members, strongest first: the cluster's face.
    pub(crate) top: Vec<(String, String, i64)>,
    /// Record kinds in the community, most frequent first.
    pub(crate) kinds: Vec<(String, usize)>,
    /// Index into the viewer's lobe dictionary: the dominant kind.
    pub(crate) lobe: usize,
    /// Edges that stay inside the community.
    pub(crate) internal_links: usize,
    /// How many members are also part of the drawn sample.
    pub(crate) sampled: usize,
    /// Every member, strongest first.
    pub(crate) members: Vec<ClusterMember>,
}

/// A corridor between two communities and its edge count.
pub(crate) struct ClusterLink {
    pub(crate) source: usize,
    pub(crate) target: usize,
    pub(crate) weight: usize,
}

/// The whole space folded into graph communities.
///
/// Built by [`cluster_index`], cached by the UI server per database
/// generation, served as `/brain-clusters.json` and `/api/cluster?id=`.
pub(crate) struct ClusterIndex {
    pub(crate) space: String,
    pub(crate) generation: String,
    pub(crate) clusters: Vec<Cluster>,
    pub(crate) links: Vec<ClusterLink>,
    pub(crate) links_truncated: bool,
    /// Sampled node id to its cluster: the drawn sample's share of the fold.
    pub(crate) node_cluster: BTreeMap<String, usize>,
    pub(crate) total_nodes: usize,
    pub(crate) total_edges: usize,
}

/// Cache key on a persistent read connection. SQLite data_version notices any
/// external commit, including in-place updates. The latest elapsed temporal
/// boundary also invalidates scheduled visibility changes without a write.
/// Counts/timestamps make revisions visible on the writing connection in tests.
/// `now` must be the same snapshot used to build the index: a transaction
/// freezes SQLite data, but cannot freeze the wall clock.
///
/// # Errors
///
/// Returns the SQLite failure when the database cannot be read.
pub(crate) fn database_generation(
    connection: &Connection,
    space: &str,
    now: i64,
) -> rusqlite::Result<String> {
    let version: i64 = connection.query_row("PRAGMA data_version", [], |row| row.get(0))?;
    let boundary: i64 = connection.query_row(
        "SELECT coalesce(max(t), 0) FROM (
          SELECT valid_from t FROM node_versions WHERE space_id=?1 UNION ALL
          SELECT recorded_at FROM node_versions WHERE space_id=?1 UNION ALL
          SELECT valid_to FROM node_versions WHERE space_id=?1 UNION ALL
          SELECT superseded_at FROM node_versions WHERE space_id=?1 UNION ALL
          SELECT valid_from FROM edge_versions WHERE space_id=?1 UNION ALL
          SELECT recorded_at FROM edge_versions WHERE space_id=?1 UNION ALL
          SELECT valid_to FROM edge_versions WHERE space_id=?1 UNION ALL
          SELECT superseded_at FROM edge_versions WHERE space_id=?1) WHERE t<=?2",
        params![space, now],
        |row| row.get(0),
    )?;
    connection.query_row(
        r#"SELECT (SELECT count(*) FROM node_versions WHERE space_id = ?1),
                  (SELECT coalesce(max(recorded_at), 0) FROM node_versions WHERE space_id = ?1),
                  (SELECT count(*) FROM edge_versions WHERE space_id = ?1),
                  (SELECT coalesce(max(recorded_at), 0) FROM edge_versions WHERE space_id = ?1)"#,
        params![space],
        |row| {
            Ok(format!(
                "v{version}-t{boundary}-n{}@{}-e{}@{}",
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )
}

/// Deterministic label propagation over the whole graph.
///
/// Every pass visits nodes in id order (the query returns them so) and moves a
/// node to the most frequent label among its neighbours, with the smallest
/// label winning ties — a rule that does not depend on the order edges arrive
/// in. Nodes without neighbours keep their own label and become singleton
/// clusters: nothing and nobody is dropped.
fn propagate_labels(adjacency: &[Vec<u32>]) -> Vec<u32> {
    let count = adjacency.len();
    let mut labels: Vec<u32> = (0..count as u32).collect();
    // Seed leaves from their only neighbour before ordered propagation. This
    // prevents a corridor visited before a star's leaves from swallowing every
    // adjacent community merely because all first-pass votes were tied.
    for (index, neighbours) in adjacency.iter().enumerate() {
        if neighbours.len() == 1 {
            labels[index] = neighbours[0];
        }
    }
    let mut frequencies: HashMap<u32, u32> = HashMap::new();
    for _ in 0..MAX_PROPAGATION_PASSES {
        let mut changed = false;
        for index in 0..count {
            let neighbours = &adjacency[index];
            if neighbours.is_empty() {
                continue;
            }
            frequencies.clear();
            for &neighbour in neighbours {
                *frequencies.entry(labels[neighbour as usize]).or_default() += 1;
            }
            // Highest frequency wins; among equals the smallest label does.
            // The comparator makes the winner unique, so scanning order cannot
            // change it.
            let winner = frequencies
                .iter()
                .max_by(|left, right| left.1.cmp(right.1).then_with(|| right.0.cmp(left.0)))
                .map_or(labels[index], |(label, _)| *label);
            if winner != labels[index] {
                labels[index] = winner;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    labels
}

/// Clusters every live node of the space, one pass over nodes and edges.
///
/// `sample_limit` says how many nodes the drawn sample carries, so the
/// document can say which part of each cluster the viewer already has.
/// `now` is the visibility snapshot used for the generation, nodes and edges.
///
/// # Errors
///
/// Returns the SQLite failure when the database cannot be read.
pub(crate) fn cluster_index(
    connection: &Connection,
    space: &str,
    sample_limit: usize,
    generation: &str,
    now: i64,
) -> rusqlite::Result<ClusterIndex> {
    // The fixed `ORDER BY n.id` is what makes everything downstream — pass
    // order, tie-breaks, cluster numbering — a function of content only.
    let mut statement = connection.prepare(
        r#"SELECT n.id, v.label, v.kind, n.degree
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)
            AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           WHERE n.space_id = ?1
           ORDER BY n.id"#,
    )?;
    let nodes = statement
        .query_map(params![space, now], |row| {
            Ok(GraphNode {
                id: row.get(0)?,
                label: row.get(1)?,
                kind: row.get(2)?,
                degree: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let position: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();

    // Every live edge as a pair of positions; ends outside the space (should
    // not happen, but the schema allows it) are skipped like in `links_among`.
    let mut statement = connection.prepare(
        r#"SELECT e.from_node_id, e.to_node_id
           FROM edges AS e
           JOIN edge_versions AS v
             ON v.space_id = e.space_id AND v.edge_id = e.id
            AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)
            AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           WHERE e.space_id = ?1"#,
    )?;
    let edges = statement.query_map(params![space, now], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); nodes.len()];
    let mut edge_pairs: Vec<(usize, usize)> = Vec::new();
    for row in edges {
        let (from, to) = row?;
        if let (Some(&a), Some(&b)) = (position.get(from.as_str()), position.get(to.as_str())) {
            adjacency[a].push(b as u32);
            adjacency[b].push(a as u32);
            edge_pairs.push((a, b));
        }
    }

    let labels = propagate_labels(&adjacency);

    // Communities, biggest first; equal sizes by their smallest member. The
    // members are pushed in id order, so member zero is the smallest id.
    let mut communities: HashMap<u32, Vec<usize>> = HashMap::new();
    for (index, label) in labels.iter().enumerate() {
        communities.entry(*label).or_default().push(index);
    }
    let mut communities: Vec<Vec<usize>> = communities.into_values().collect();
    communities.sort_by(|left, right| {
        right
            .len()
            .cmp(&left.len())
            .then_with(|| left[0].cmp(&right[0]))
    });
    let cluster_of: Vec<usize> = {
        let mut cluster_of = vec![0_usize; nodes.len()];
        for (id, members) in communities.iter().enumerate() {
            for &member in members {
                cluster_of[member] = id;
            }
        }
        cluster_of
    };

    let mut internal_links = vec![0_usize; communities.len()];
    let mut between: HashMap<(usize, usize), usize> = HashMap::new();
    for (a, b) in &edge_pairs {
        let (ca, cb) = (cluster_of[*a], cluster_of[*b]);
        if ca == cb {
            internal_links[ca] += 1;
        } else {
            *between.entry((ca.min(cb), ca.max(cb))).or_default() += 1;
        }
    }
    let mut links: Vec<(usize, usize, usize)> = between
        .into_iter()
        .map(|((a, b), weight)| (a, b, weight))
        .collect();
    links.sort_by(|left, right| {
        right
            .2
            .cmp(&left.2)
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.1.cmp(&right.1))
    });
    let links_truncated = links.len() > MAX_CLUSTER_LINKS;
    links.truncate(MAX_CLUSTER_LINKS);

    // The drawn sample, selected by the same rule `document()` uses, so the
    // viewer can map every drawn node onto its community.
    let mut statement = connection.prepare(
        r#"SELECT n.id
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.recorded_at <= ?2 AND (v.superseded_at IS NULL OR ?2 < v.superseded_at)
            AND v.valid_from <= ?2 AND (v.valid_to IS NULL OR ?2 < v.valid_to)
           WHERE n.space_id = ?1
           ORDER BY n.degree DESC, n.salience DESC, n.id
           LIMIT ?3"#,
    )?;
    let sample = statement
        .query_map(
            params![space, now, sample_limit.clamp(1, MAX_LIMIT) as i64],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<HashSet<_>>>()?;

    let clusters = communities
        .into_iter()
        .enumerate()
        .map(|(id, members)| {
            let mut strongest = members;
            // The face of the cluster: strongest first, ties to the smaller id.
            strongest.sort_by(|left, right| {
                nodes[*right]
                    .degree
                    .cmp(&nodes[*left].degree)
                    .then_with(|| left.cmp(right))
            });
            let top = strongest
                .iter()
                .take(3)
                .map(|&index| {
                    (
                        nodes[index].id.clone(),
                        shorten(&nodes[index].label),
                        nodes[index].degree,
                    )
                })
                .collect::<Vec<_>>();
            let label = top
                .first()
                .map_or_else(String::new, |(_, label, _)| label.clone());
            let mut kinds: HashMap<&str, usize> = HashMap::new();
            for &index in &strongest {
                *kinds.entry(nodes[index].kind.as_str()).or_default() += 1;
            }
            let mut kinds: Vec<(String, usize)> = kinds
                .into_iter()
                .map(|(kind, count)| (kind.to_owned(), count))
                .collect();
            kinds.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            let lobe = kinds.first().map_or(0, |(kind, _)| kind_index(kind));
            let members = strongest
                .into_iter()
                .map(|index| ClusterMember {
                    id: nodes[index].id.clone(),
                    label: shorten(&nodes[index].label),
                    kind: nodes[index].kind.clone(),
                    degree: nodes[index].degree,
                })
                .collect::<Vec<_>>();
            let sampled = members.iter().filter(|m| sample.contains(&m.id)).count();
            Cluster {
                size: members.len(),
                label,
                top,
                kinds,
                lobe,
                internal_links: internal_links[id],
                sampled,
                members,
            }
        })
        .collect::<Vec<_>>();

    let node_cluster = sample
        .into_iter()
        .filter_map(|id| {
            position
                .get(id.as_str())
                .map(|&index| (id, cluster_of[index]))
        })
        .collect::<BTreeMap<_, _>>();

    Ok(ClusterIndex {
        space: space.to_owned(),
        generation: generation.to_owned(),
        clusters,
        links: links
            .into_iter()
            .map(|(source, target, weight)| ClusterLink {
                source,
                target,
                weight,
            })
            .collect(),
        links_truncated,
        node_cluster,
        total_nodes: nodes.len(),
        total_edges: edge_pairs.len(),
    })
}

impl ClusterIndex {
    /// The document behind `/brain-clusters.json`: every node of the space in
    /// exactly one community, the corridors between them, and the sample
    /// overlap the drawn graph can use.
    pub(crate) fn clusters_document(&self) -> Value {
        let clusters = self
            .clusters
            .iter()
            .enumerate()
            .map(|(id, cluster)| {
                json!({
                    "id": id,
                    "size": cluster.size,
                    "label": cluster.label,
                    "top": cluster
                        .top
                        .iter()
                        .map(|(id, label, degree)| json!({
                            "id": id, "label": label, "degree": degree,
                        }))
                        .collect::<Vec<_>>(),
                    "kinds": cluster
                        .kinds
                        .iter()
                        .map(|(kind, count)| (kind.clone(), json!(count)))
                        .collect::<serde_json::Map<String, Value>>(),
                    "lobe": cluster.lobe,
                    "internal_links": cluster.internal_links,
                    "sampled": cluster.sampled,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "format": "brain-clusters-v1",
            "space": self.space,
            "generation": self.generation,
            "total_nodes": self.total_nodes,
            "total_edges": self.total_edges,
            "clusters": clusters,
            "links": self
                .links
                .iter()
                .map(|link| json!({
                    "source": link.source, "target": link.target, "weight": link.weight,
                }))
                .collect::<Vec<_>>(),
            "links_truncated": self.links_truncated,
            "node_clusters": self.node_cluster,
        })
    }

    /// One cluster expanded by id (`/api/cluster?id=`): the strongest members
    /// first, each row carrying its id so the viewer can load it further.
    pub(crate) fn cluster_detail(&self, id: usize, offset: usize) -> Option<Value> {
        let cluster = self.clusters.get(id)?;
        let rows = cluster
            .members
            .iter()
            .skip(offset)
            .take(CLUSTER_DETAIL_ROWS)
            .map(|member| {
                json!([
                    member.id,
                    member.label,
                    member.kind,
                    member.degree,
                    self.node_cluster.contains_key(&member.id),
                ])
            })
            .collect::<Vec<_>>();
        let corridors = self
            .links
            .iter()
            .filter(|link| link.source == id || link.target == id)
            .take(24)
            .map(|link| {
                json!({
                    "target": if link.source == id { link.target } else { link.source },
                    "weight": link.weight,
                })
            })
            .collect::<Vec<_>>();
        Some(json!({
            "format": "brain-cluster-v1",
            "id": id,
            "label": cluster.label,
            "size": cluster.size,
            "internal_links": cluster.internal_links,
            "sampled": cluster.sampled,
            "offset": offset,
            "generation": self.generation,
            "returned": rows.len(),
            "nodes": rows,
            "links": corridors,
        }))
    }
}

pub(crate) fn open_read_only(database: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_label_is_cut_with_a_mark_that_says_so() {
        let long = "и".repeat(200);
        let short = shorten(&long);

        assert!(short.chars().count() <= 64);
        assert!(short.ends_with('…'), "a cut label must show it was cut");
    }

    #[test]
    fn a_short_label_survives_untouched() {
        assert_eq!(shorten("  Память  "), "Память");
    }

    /// The viewer reads node fields by position, so a row that does not match
    /// the schema length misaligns every column after the mistake.
    #[test]
    fn search_document_finds_chain_nodes_by_words() {
        let (_scratch, database) = crate::explore::tests::chain_database(11);
        let found = search_document(&database, "default", "zebra", 10).expect("search");
        let rows = found.as_array().expect("array");
        assert!(!rows.is_empty(), "{found}");
        assert_eq!(rows[0]["id"], "n10");
    }

    #[test]
    fn the_schema_and_the_rows_agree_on_length() {
        assert_eq!(NODE_FIELDS.len(), 18);
        assert_eq!(LINK_FIELDS.len(), 4);
    }

    #[test]
    fn seed_positions_are_spread_and_repeatable() {
        let first = seed_position(0, 100);
        let again = seed_position(0, 100);
        let other = seed_position(50, 100);

        assert_eq!(first, again, "the same memory must look the same twice");
        assert!(
            (first.0 - other.0).abs() + (first.1 - other.1).abs() + (first.2 - other.2).abs() > 1.0,
            "different nodes must not start in the same place"
        );
    }

    /// Two triangles joined by a long chain, plus two orphans: label
    /// propagation folds the a-side and the chain into one community, keeps
    /// the b-side apart (the chain is longer than the pass cap), and the
    /// orphans stay on their own. Enough shape to check by hand.
    fn component_database(
        chain_between_sides: bool,
    ) -> (crate::explore::tests::Scratch, std::path::PathBuf) {
        component_database_order(chain_between_sides, false)
    }

    fn component_database_order(
        chain_between_sides: bool,
        shuffled: bool,
    ) -> (crate::explore::tests::Scratch, std::path::PathBuf) {
        let directory = crate::test_support::temp_path("brain-graph-clusters");
        std::fs::create_dir_all(&directory).expect("temporary directory");
        let path = directory.join("brain.sqlite");
        drop(brain_core::SqliteStorage::open(&path).expect("migrate"));
        let connection = Connection::open(&path).expect("open");
        connection
            .execute_batch(
                "INSERT INTO agents(id, display_name, created_at) VALUES ('t', 't', 1);
                 INSERT INTO spaces(id, kind, label, created_by_agent_id, created_at)
                   VALUES ('default', 'project', 'default', 't', 1);
                 INSERT INTO sessions(id, space_id, agent_id, started_at) VALUES ('s', 'default', 't', 1);",
            )
            .expect("seed");
        let mut nodes: Vec<(String, i64, &str)> = vec![
            ("a1".into(), 3, "alpha one"),
            ("a2".into(), 2, "alpha two"),
            ("a3".into(), 3, "alpha three"),
            ("a4".into(), 1, "alpha four"),
            ("b1".into(), 3, "beta one"),
            ("b2".into(), 2, "beta two"),
            ("b3".into(), 2, "beta three"),
            ("o1".into(), 0, "orphan one"),
            ("o2".into(), 0, "orphan two"),
        ];
        if chain_between_sides {
            for index in 1..=13 {
                nodes.push((format!("c{index:02}"), 2, "chain link"));
            }
        }
        // The second fixture changes insertion order: the fold must come
        // from the query's ORDER BY, not from row order.
        let mut order: Vec<usize> = (0..nodes.len()).collect();
        if shuffled {
            order.reverse();
            order.rotate_left(7);
        }
        for &offset in &order {
            let (id, degree, label) = nodes[offset].clone();
            connection
                .execute(
                    "INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id,
                                       created_at, last_accessed, degree)
                     VALUES (?1, 'default', 't', 's', 1, 1, ?2)",
                    params![id, degree],
                )
                .expect("node");
            connection
                .execute(
                    "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, source,
                                               valid_from, recorded_at, agent_id, session_id)
                     VALUES (?1, 'default', 1, 'concept', ?2, 'body', 'synthetic', 1, 1, 't', 's')",
                    params![id, label],
                )
                .expect("version");
        }
        let mut edges: Vec<(String, String)> = vec![
            ("a1".into(), "a2".into()),
            ("a2".into(), "a3".into()),
            ("a3".into(), "a1".into()),
            ("a4".into(), "a1".into()),
            ("b1".into(), "b2".into()),
            ("b2".into(), "b3".into()),
            ("b3".into(), "b1".into()),
        ];
        if chain_between_sides {
            edges.push(("a3".into(), "c01".into()));
            for index in 1..13 {
                edges.push((format!("c{index:02}"), format!("c{:02}", index + 1)));
            }
            edges.push(("c13".into(), "b1".into()));
        }
        if shuffled {
            edges.reverse();
        }
        for (index, (from, to)) in edges.iter().enumerate() {
            // Half the edges in reverse direction: the fold must not depend on
            // which end was written first.
            let (from, to) = if (index % 2 == 0) != shuffled {
                (from.clone(), to.clone())
            } else {
                (to.clone(), from.clone())
            };
            connection
                .execute(
                    "INSERT INTO edges(id, space_id, from_node_id, to_node_id, created_by_agent_id,
                                       created_in_session_id, created_at, last_accessed)
                     VALUES (?1, 'default', ?2, ?3, 't', 's', 1, 1)",
                    params![format!("e{index}"), from, to],
                )
                .expect("edge");
            connection
                .execute(
                    "INSERT INTO edge_versions(edge_id, space_id, revision, relation, weight, confidence,
                                               valid_from, recorded_at, agent_id, session_id)
                     VALUES (?1, 'default', 1, 'relates_to', 0.5, 0.9, 1, 1, 't', 's')",
                    params![format!("e{index}")],
                )
                .expect("edge version");
        }
        (crate::explore::tests::Scratch(directory), path)
    }

    fn fold(database: &Path) -> ClusterIndex {
        let now = crate::now_unix_seconds();
        let connection = open_read_only(database).expect("open");
        let generation = database_generation(&connection, "default", now).expect("generation");
        cluster_index(&connection, "default", DEFAULT_LIMIT, &generation, now).expect("index")
    }

    /// The fold is a partition: every node in exactly one community, and the
    /// same base folded twice gives the same answer byte for byte.
    #[test]
    fn clusters_cover_every_node_exactly_once_and_repeat_themselves() {
        let (_scratch, database) = component_database(false);
        let first = fold(&database);
        let second = fold(&database);
        assert_eq!(
            first.clusters_document().to_string(),
            second.clusters_document().to_string(),
            "the same base must fold the same way twice"
        );

        let mut members: Vec<&str> = first
            .clusters
            .iter()
            .flat_map(|cluster| cluster.members.iter().map(|member| member.id.as_str()))
            .collect();
        let total: usize = first.clusters.iter().map(|cluster| cluster.size).sum();
        assert_eq!(total, first.total_nodes, "sizes must add up to the space");
        members.sort_unstable();
        assert_eq!(
            members.windows(2).filter(|pair| pair[0] == pair[1]).count(),
            0,
            "no node may land in two clusters"
        );
        assert_eq!(
            members,
            ["a1", "a2", "a3", "a4", "b1", "b2", "b3", "o1", "o2"],
            "every node must be covered"
        );

        let sizes: Vec<usize> = first.clusters.iter().map(|cluster| cluster.size).collect();
        assert_eq!(sizes, [4, 3, 1, 1], "one community per part, orphans apart");
        assert_eq!(
            first.clusters[0].top[0].0, "a1",
            "the strongest member fronts the cluster"
        );
        assert_eq!(first.clusters[0].internal_links, 4);
        assert_eq!(first.clusters[1].label, "beta one");
        assert_eq!(first.total_edges, 7);
    }

    /// A node nobody links to keeps its own label: it becomes a singleton
    /// cluster rather than vanishing between the others.
    #[test]
    fn orphan_nodes_become_singletons_and_are_not_lost() {
        let (_scratch, database) = component_database(false);
        let index = fold(&database);
        let orphans: Vec<&Cluster> = index
            .clusters
            .iter()
            .filter(|cluster| cluster.size == 1)
            .collect();
        assert_eq!(orphans.len(), 2, "both orphans stay alone");
        let mut ids: Vec<&str> = orphans
            .iter()
            .map(|cluster| cluster.members[0].id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["o1", "o2"]);
        for orphan in &orphans {
            assert_eq!(orphan.internal_links, 0);
            assert_eq!(orphan.sampled, 1, "an orphan is part of the sample too");
        }
        let document = index.clusters_document();
        let mapped = &document["node_clusters"];
        assert_eq!(mapped["o1"], 2, "the sample map must carry the orphans");
        assert_eq!(mapped["o2"], 3);
    }

    /// The same graph with edges written in the other direction and a
    /// different row order must fold identically: no dependence on write order.
    #[test]
    fn the_fold_does_not_depend_on_write_order() {
        let (_scratch, plain) = component_database(false);
        let (_reversed, other) = component_database_order(false, true);
        let plain = fold(&plain);
        let other = fold(&other);
        let members = |index: &ClusterIndex| -> Vec<Vec<String>> {
            index
                .clusters
                .iter()
                .map(|cluster| cluster.members.iter().map(|m| m.id.clone()).collect())
                .collect()
        };
        assert_eq!(
            plain.clusters.iter().map(|c| c.size).collect::<Vec<_>>(),
            [4, 3, 1, 1]
        );
        assert_eq!(members(&plain), members(&other));
        assert_eq!(
            plain.clusters_document().to_string(),
            other.clusters_document().to_string()
        );
    }

    /// Two parts the pass cap keeps apart still report the corridor between
    /// them, and the cluster expands by id with its members.
    #[test]
    fn a_fold_with_a_corridor_lists_it_and_a_cluster_expands_by_id() {
        let (_scratch, database) = component_database(true);
        let index = fold(&database);
        let sizes: Vec<usize> = index.clusters.iter().map(|cluster| cluster.size).collect();
        assert_eq!(
            sizes,
            [17, 3, 1, 1],
            "the chain joins the a-side within the pass cap"
        );
        assert_eq!(index.total_edges, 21);
        assert_eq!(
            index.links.len(),
            1,
            "one corridor between the two communities"
        );
        let corridor = &index.links[0];
        assert_eq!(
            (corridor.source, corridor.target, corridor.weight),
            (0, 1, 1)
        );
        assert!(!index.links_truncated);

        let detail = index.cluster_detail(0, 0).expect("cluster 0 exists");
        assert_eq!(detail["format"], "brain-cluster-v1");
        assert_eq!(detail["size"], 17);
        assert_eq!(detail["returned"], CLUSTER_DETAIL_ROWS.min(17));
        let rows = detail["nodes"].as_array().expect("rows");
        assert_eq!(rows[0][0], "a1", "strongest member first");
        assert_eq!(rows[0][4], true, "the sample map knows this member");
        assert!(
            rows.iter().all(|row| row[0] != "o1" && row[0] != "o2"),
            "orphans remain separate"
        );
        let corridors = detail["links"].as_array().expect("corridors");
        assert_eq!(corridors.len(), 1);
        assert_eq!(corridors[0]["target"], 1);

        assert!(index.cluster_detail(99, 0).is_none(), "no such cluster");
        let document = index.clusters_document();
        assert_eq!(document["format"], "brain-clusters-v1");
        assert_eq!(document["clusters"].as_array().unwrap().len(), 4);
        // A tight sample still maps only what the drawn sample carries.
        let now = crate::now_unix_seconds();
        let connection = open_read_only(&database).expect("open");
        let generation = database_generation(&connection, "default", now).unwrap();
        let narrow = cluster_index(&connection, "default", 2, &generation, now).unwrap();
        assert_eq!(
            narrow.node_cluster,
            BTreeMap::from([("a1".to_owned(), 0), ("a3".to_owned(), 0)]),
            "the two strongest nodes, both from cluster 0"
        );
        assert_eq!(narrow.clusters[0].sampled, 2);
        assert_eq!(narrow.clusters[1].sampled, 0);
    }

    #[test]
    fn clusters_exclude_forgotten_future_and_other_spaces() {
        let (_scratch, database) = component_database(false);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute("UPDATE node_versions SET valid_to=2 WHERE node_id='a1'", [])
            .unwrap();
        connection
            .execute(
                "UPDATE node_versions SET recorded_at=?1 WHERE node_id='b1'",
                params![crate::now_unix_seconds() + 3600],
            )
            .unwrap();
        connection.execute_batch("INSERT INTO spaces(id,kind,label,created_by_agent_id,created_at) VALUES ('private','project','private','t',1);
            INSERT INTO sessions(id,space_id,agent_id,started_at) VALUES ('sp','private','t',1);
            INSERT INTO nodes(id,space_id,created_by_agent_id,created_in_session_id,created_at,last_accessed) VALUES ('secret','private','t','sp',1,1);
            INSERT INTO node_versions(node_id,space_id,revision,kind,label,body,source,valid_from,recorded_at,agent_id,session_id) VALUES ('secret','private',1,'concept','private','body','synthetic',1,1,'t','sp');").unwrap();
        let index = fold(&database);
        let ids: Vec<_> = index
            .clusters
            .iter()
            .flat_map(|c| c.members.iter().map(|m| m.id.as_str()))
            .collect();
        assert_eq!(ids.len(), 7);
        for excluded in ["a1", "b1", "secret"] {
            assert!(!ids.contains(&excluded));
        }
        assert_eq!(
            index.total_edges, 2,
            "edges to invisible endpoints disappear"
        );
    }

    #[test]
    fn cluster_pages_reach_every_member_and_selected_graph_stays_scoped() {
        let (_scratch, database) = crate::explore::tests::chain_database(1101);
        let index = fold(&database);
        let mut all = std::collections::BTreeSet::new();
        for (id, cluster) in index.clusters.iter().enumerate() {
            for offset in (0..cluster.size).step_by(CLUSTER_DETAIL_ROWS) {
                let detail = index.cluster_detail(id, offset).unwrap();
                let ids: Vec<String> = detail["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|r| r[0].as_str().unwrap().to_owned())
                    .collect();
                assert!(ids.len() <= CLUSTER_DETAIL_ROWS);
                let graph =
                    selected_document(&database, "default", CLUSTER_DETAIL_ROWS, Some(&ids))
                        .unwrap();
                assert_eq!(graph["nodes"].as_array().unwrap().len(), ids.len());
                for row in graph["nodes"].as_array().unwrap() {
                    assert!(ids.contains(&row[0].as_str().unwrap().to_owned()));
                }
                for id in ids {
                    assert!(all.insert(id), "no overlap across pages");
                }
            }
        }
        assert_eq!(all.len(), 1101);
    }

    /// The cache key must move when the space's content moves, or the viewer
    /// would keep showing a fold of a base that no longer exists.
    #[test]
    fn the_generation_moves_when_the_space_changes() {
        let now = crate::now_unix_seconds();
        let (_scratch, database) = component_database(false);
        let connection = Connection::open(&database).expect("open");
        let before = database_generation(&connection, "default", now).unwrap();
        connection
            .execute(
                "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, source,
                                           valid_from, recorded_at, agent_id, session_id)
                 VALUES ('a1', 'default', 2, 'concept', 'alpha one revised', 'body', 'synthetic', 1, 2, 't', 's')",
                [],
            )
            .expect("revision");
        let after = database_generation(&connection, "default", now).unwrap();
        assert_ne!(before, after, "a revision must move the generation");
        assert_eq!(
            after,
            database_generation(&connection, "default", now).unwrap()
        );
    }
}
