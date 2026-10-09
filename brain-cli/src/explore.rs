//! The whole graph, a page at a time.
//!
//! The 3D view draws the 4,000 most connected nodes because a laptop cannot
//! animate ten thousand. That left most of the memory unreachable from the
//! window: a node outside the sample could not be found, opened, or followed.
//! These three documents close that gap without drawing more — an overview of
//! everything stored, a filtered list that pages through all of it, and one
//! node with its neighbours — so the page renders only what someone asked for.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

/// Kinds the schema allows; anything else in a filter is a client error.
pub(crate) const KINDS: [&str; 8] = [
    "code", "document", "memory", "session", "decision", "error", "concept", "entity",
];

/// Rows one list page may carry.
const MAX_PAGE: usize = 200;
const DEFAULT_PAGE: usize = 50;

/// Neighbours one node page may carry.
const MAX_NEIGHBOURS: usize = 100;
const DEFAULT_NEIGHBOURS: usize = 40;

/// Search hits considered before paging; a broader query should be narrowed.
const MAX_MATCHES: usize = 5_000;

/// Body text sent for one node. The rest stays in the database.
const BODY_EXCERPT_CHARS: usize = 4_000;

/// Source groups listed by name in the overview; the tail is summed.
const LISTED_GROUPS: usize = 40;

/// Longest node id accepted in a request.
const MAX_ID_BYTES: usize = 4_096;

/// Label length in list rows.
const LABEL_CHARS: usize = 160;

/// Nodes one drawn selection may carry, and the default when unspecified.
pub(crate) const MAX_DRAWN: usize = 1_000;
const DEFAULT_DRAWN: usize = 400;

/// Edges one answer carries among drawn nodes; the exact total is reported.
const MAX_DRAWN_EDGES: usize = 8_000;

/// Clusters listed by name; smaller ones are summed into one "rest" entry.
const LISTED_CLUSTERS: usize = 48;

/// Cluster-to-cluster pairs listed, strongest first.
const LISTED_CLUSTER_LINKS: usize = 400;

/// Label length on the canvas and in its tooltips.
const DRAWN_LABEL_CHARS: usize = 120;

/// Why a document could not be built.
#[derive(Debug)]
pub(crate) enum ExploreError {
    BadRequest(String),
    NotFound(String),
    Database(rusqlite::Error),
}

impl From<rusqlite::Error> for ExploreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

/// Decoded query-string parameters. Later duplicates win.
pub(crate) fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'+' => out.push(b' '),
            b'%' if position + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[position + 1..position + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        position += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        position += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn open(database: &Path) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn number(
    params: &HashMap<String, String>,
    key: &str,
    default: usize,
    ceiling: usize,
) -> Result<usize, ExploreError> {
    match params.get(key).map(String::as_str) {
        None | Some("") => Ok(default),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|value| *value <= ceiling)
            .ok_or_else(|| {
                ExploreError::BadRequest(format!("{key}: ожидается целое число от 0 до {ceiling}"))
            }),
    }
}

/// One current node, as much as a list row or the overview needs.
struct Entry {
    id: String,
    label: String,
    kind: String,
    source: Option<String>,
    degree: i64,
    salience: f64,
    layer: String,
    created_at: i64,
}

fn entries(connection: &Connection, space: &str) -> rusqlite::Result<Vec<Entry>> {
    let mut statement = connection.prepare(
        r#"SELECT n.id, v.label, v.kind, v.source, v.agent_id, n.degree, n.salience,
                  n.salience_layer, n.created_at
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.superseded_at IS NULL
           WHERE n.space_id = ?1"#,
    )?;
    statement
        .query_map([space], |row| {
            Ok(Entry {
                id: row.get(0)?,
                label: row.get(1)?,
                kind: row.get(2)?,
                source: row.get(3)?,
                degree: row.get(5)?,
                salience: row.get(6)?,
                layer: row.get(7)?,
                created_at: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map(|mut rows| {
            // One current version per node is the writer's invariant; if a
            // crash ever left two, count the node once rather than twice.
            let mut seen = HashSet::new();
            rows.retain(|entry: &Entry| seen.insert(entry.id.clone()));
            rows
        })
}

/// Current edges of a space as endpoint pairs, each logical edge once.
///
/// The `nodes.degree` column is a cache the importer does not maintain — on a
/// real ten-thousand-node graph it read 0 for all but 65 nodes while 31,000
/// edges existed — so anything that ranks or shows "links" counts them here.
fn edge_pairs(connection: &Connection, space: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let mut statement = connection.prepare(
        r#"SELECT e.from_node_id, e.to_node_id FROM edges AS e
           WHERE e.space_id = ?1
             AND EXISTS (SELECT 1 FROM edge_versions AS v
                         WHERE v.edge_id = e.id AND v.space_id = e.space_id
                           AND v.superseded_at IS NULL)"#,
    )?;
    statement
        .query_map([space], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Links per node, counted from current edges.
fn live_degrees(pairs: &[(String, String)]) -> HashMap<&str, i64> {
    let mut degrees: HashMap<&str, i64> = HashMap::new();
    for (from, to) in pairs {
        *degrees.entry(from.as_str()).or_default() += 1;
        *degrees.entry(to.as_str()).or_default() += 1;
    }
    degrees
}

/// A short, stable name for where a node came from.
///
/// Sources are file paths, tool names, or nothing. Grouping by the first
/// meaningful directory turns ten thousand paths into a few dozen piles a
/// person recognises ("notes-hub", ".codex/sessions") without inventing
/// a project structure the data does not carry.
pub(crate) fn source_group(source: Option<&str>) -> String {
    let Some(source) = source.map(str::trim).filter(|text| !text.is_empty()) else {
        return "(без источника)".to_owned();
    };
    let segments = source
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return "(без источника)".to_owned();
    }
    let absolute = source.starts_with('/');
    let rest: &[&str] = if absolute && segments[0] == "home" && segments.len() > 2 {
        &segments[2..]
    } else {
        &segments[..]
    };
    let group = if absolute && segments[0] != "home" {
        format!(
            "/{}",
            segments
                .iter()
                .take(2)
                .copied()
                .collect::<Vec<_>>()
                .join("/")
        )
    } else if rest.len() > 1 && rest[0].starts_with('.') {
        format!("{}/{}", rest[0], rest[1])
    } else if rest.len() > 1 || absolute {
        rest[0].to_owned()
    } else {
        // A bare name is a tool or importer, not a directory.
        rest[0].to_owned()
    };
    shorten(&group, 60)
}

fn shorten(text: &str, limit: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= limit {
        return single_line;
    }
    let mut cut = single_line
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    cut.push('…');
    cut
}

fn row_document(entry: &Entry) -> Value {
    json!({
        "id": entry.id,
        "label": shorten(&entry.label, LABEL_CHARS),
        "kind": entry.kind,
        "source": entry.source.as_deref().map(|source| shorten(source, 200)),
        "group": source_group(entry.source.as_deref()),
        "degree": entry.degree,
        "salience": (entry.salience * 1000.0).round() / 1000.0,
        "layer": entry.layer,
        "created_at": entry.created_at,
    })
}

/// The space a request asked for, checked against the spaces actually stored.
///
/// # Errors
///
/// `BadRequest` for a name that is not a stored space.
pub(crate) fn resolve_space(
    database: &Path,
    params: &HashMap<String, String>,
    default: &str,
) -> Result<String, ExploreError> {
    let Some(requested) = params
        .get("space")
        .map(String::as_str)
        .filter(|space| !space.is_empty())
    else {
        return Ok(default.to_owned());
    };
    if requested.len() > 200 {
        return Err(ExploreError::BadRequest(
            "слишком длинное имя пространства".to_owned(),
        ));
    }
    let connection = open(database)?;
    let known: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM spaces WHERE id = ?1)",
        [requested],
        |row| row.get(0),
    )?;
    if known {
        Ok(requested.to_owned())
    } else {
        Err(ExploreError::BadRequest(format!(
            "нет такого пространства: {requested}"
        )))
    }
}

/// Everything stored, counted — never a sample.
///
/// # Errors
///
/// Returns the SQLite failure when the database cannot be read.
/// Rows the overview breakdown reads before it stops and reports truncation.
const MAX_OVERVIEW_SCAN: usize = 1_000_000;
/// Distinct buckets kept per breakdown; further values are counted, not named.
const MAX_OVERVIEW_BUCKETS: usize = 4_096;

/// Nodes that a capped breakdown could not attribute to a named bucket.
#[derive(Default)]
struct OverviewOverflow {
    scan: bool,
    kinds: usize,
    agents: usize,
    groups: usize,
    months: usize,
}

impl OverviewOverflow {
    fn any(&self) -> bool {
        self.scan || self.kinds + self.agents + self.groups + self.months > 0
    }

    fn report(&self) -> Value {
        json!({
            "scan_limit": MAX_OVERVIEW_SCAN,
            "scan_limit_reached": self.scan,
            "bucket_limit": MAX_OVERVIEW_BUCKETS,
            "unbucketed_kinds": self.kinds,
            "unbucketed_agents": self.agents,
            "unbucketed_groups": self.groups,
            "unbucketed_months": self.months,
        })
    }
}

fn bump(map: &mut BTreeMap<String, usize>, key: String, overflow: &mut usize) {
    if let Some(count) = map.get_mut(&key) {
        *count += 1;
    } else if map.len() < MAX_OVERVIEW_BUCKETS {
        map.insert(key, 1);
    } else {
        *overflow += 1;
    }
}

fn bump_hashed(map: &mut HashMap<String, usize>, key: String, overflow: &mut usize) {
    if let Some(count) = map.get_mut(&key) {
        *count += 1;
    } else if map.len() < MAX_OVERVIEW_BUCKETS {
        map.insert(key, 1);
    } else {
        *overflow += 1;
    }
}

pub(crate) fn overview(database: &Path, space: &str) -> Result<Value, ExploreError> {
    let connection = open(database)?;
    // Exact total from SQL; the per-bucket breakdown below is streamed row by
    // row instead of loading every node, and both the scan and the number of
    // distinct buckets are capped. When a cap bites the answer says so.
    let total_nodes: i64 = connection.query_row(
        r#"SELECT count(DISTINCT n.id) FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.superseded_at IS NULL
           WHERE n.space_id = ?1"#,
        [space],
        |row| row.get(0),
    )?;
    let total_edges: i64 = connection.query_row(
        r#"SELECT count(*) FROM edges AS e
           JOIN edge_versions AS v
             ON v.space_id = e.space_id AND v.edge_id = e.id AND v.superseded_at IS NULL
           WHERE e.space_id = ?1"#,
        [space],
        |row| row.get(0),
    )?;

    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut agents: BTreeMap<String, usize> = BTreeMap::new();
    let mut groups: HashMap<String, usize> = HashMap::new();
    let mut months: BTreeMap<String, usize> = BTreeMap::new();
    let mut overflow = OverviewOverflow::default();
    let mut scanned = 0_usize;
    let mut statement = connection.prepare(
        r#"SELECT n.id, v.kind, v.agent_id, v.source, n.created_at
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id
            AND v.superseded_at IS NULL
           WHERE n.space_id = ?1
           ORDER BY n.id"#,
    )?;
    let mut rows = statement.query([space])?;
    let mut previous: Option<String> = None;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        // One current version per node is the writer's invariant; if a crash
        // ever left two, count the node once rather than twice.
        if previous.as_deref() == Some(id.as_str()) {
            continue;
        }
        if scanned >= MAX_OVERVIEW_SCAN {
            overflow.scan = true;
            break;
        }
        scanned += 1;
        previous = Some(id);
        let source: Option<String> = row.get(3)?;
        bump(&mut kinds, row.get(1)?, &mut overflow.kinds);
        bump(&mut agents, row.get(2)?, &mut overflow.agents);
        bump_hashed(
            &mut groups,
            source_group(source.as_deref()),
            &mut overflow.groups,
        );
        bump(&mut months, month_of(row.get(4)?), &mut overflow.months);
    }
    drop(rows);
    drop(statement);
    let mut ranked = groups.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let unlisted = ranked
        .iter()
        .skip(LISTED_GROUPS)
        .map(|(_, count)| count)
        .sum::<usize>();
    let group_count = ranked.len();
    ranked.truncate(LISTED_GROUPS);

    let mut relations_statement = connection.prepare(
        r#"SELECT v.relation, count(*) FROM edges AS e
           JOIN edge_versions AS v
             ON v.space_id = e.space_id AND v.edge_id = e.id AND v.superseded_at IS NULL
           WHERE e.space_id = ?1 GROUP BY v.relation ORDER BY count(*) DESC LIMIT 20"#,
    )?;
    let relations = relations_statement
        .query_map([space], |row| {
            Ok(json!({"relation": row.get::<_, String>(0)?, "count": row.get::<_, i64>(1)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut spaces_statement = connection.prepare(
        r#"SELECT s.id, (SELECT count(*) FROM nodes AS n WHERE n.space_id = s.id) AS total
           FROM spaces AS s ORDER BY total DESC, s.id"#,
    )?;
    let spaces = spaces_statement
        .query_map([], |row| {
            Ok(json!({"space": row.get::<_, String>(0)?, "nodes": row.get::<_, i64>(1)?}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(json!({
        "space": space,
        "total_nodes": total_nodes,
        "scanned_nodes": scanned,
        "truncated": overflow.any(),
        "truncation": overflow.report(),
        "total_edges": total_edges,
        "kinds": kinds,
        "agents": agents,
        "groups": ranked.into_iter().map(|(group, count)| json!({"group": group, "count": count})).collect::<Vec<_>>(),
        "group_count": group_count,
        "unlisted_group_nodes": unlisted,
        "relations": relations,
        "timeline": months.into_iter().map(|(month, count)| json!({"month": month, "count": count})).collect::<Vec<_>>(),
        "timeline_meaning": "месяц, когда узел был записан в Brain, а не когда произошло событие",
        "spaces": spaces,
        "scope": [
            "Здесь всё, что хранится в базе Brain для этого пространства; список и поиск проходят по всем узлам.",
            "Полных переписок здесь нет: автоматический захват транскриптов не реализован.",
            "Пространства не смешиваются: переключатель вверху показывает по одному; старый граф Graphify (порт 8765) и приватный архив сессий — отдельные хранилища, сюда не входят.",
            "Группы — первая папка пути источника, а не проекты, размеченные человеком."
        ],
    }))
}

fn month_of(timestamp: i64) -> String {
    // Civil-from-days (Howard Hinnant), so no date crate is needed for a label.
    let days = timestamp.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}")
}

/// Node ids matching a free-text query, best first.
///
/// Full-text search covers label, body and source; a plain substring match on
/// the label catches what the tokenizer splits apart, such as path fragments.
fn matching_ids(
    connection: &Connection,
    space: &str,
    query: &str,
) -> rusqlite::Result<(Vec<String>, bool)> {
    let terms = query
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"*", term.to_lowercase().replace('"', "\"\"")))
        .collect::<Vec<_>>();
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    if !terms.is_empty() {
        let mut statement = connection.prepare(
            r#"SELECT v.node_id FROM node_versions_fts AS f
               JOIN node_versions AS v ON v.row_id = f.rowid
               WHERE node_versions_fts MATCH ?1 AND v.space_id = ?2 AND v.superseded_at IS NULL
               ORDER BY bm25(node_versions_fts, 4.0, 1.0, 1.0)
               LIMIT ?3"#,
        )?;
        let rows = statement.query_map(
            params![terms.join(" "), space, (MAX_MATCHES + 1) as i64],
            |row| row.get::<_, String>(0),
        )?;
        for id in rows {
            let id = id?;
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
    }
    let pattern = format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let mut statement = connection.prepare(
        r#"SELECT node_id FROM node_versions
           WHERE space_id = ?1 AND superseded_at IS NULL AND label LIKE ?2 ESCAPE '\'
           LIMIT ?3"#,
    )?;
    let rows = statement.query_map(params![space, pattern, (MAX_MATCHES + 1) as i64], |row| {
        row.get::<_, String>(0)
    })?;
    for id in rows {
        let id = id?;
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    let capped = ids.len() > MAX_MATCHES;
    ids.truncate(MAX_MATCHES);
    Ok((ids, capped))
}

/// One page of nodes, filtered, out of all of them.
///
/// # Errors
///
/// `BadRequest` for an unknown kind, an out-of-range number, or an overlong
/// query; the SQLite failure when the database cannot be read.
pub(crate) fn nodes(
    database: &Path,
    space: &str,
    params: &HashMap<String, String>,
) -> Result<Value, ExploreError> {
    let offset = number(params, "offset", 0, 1_000_000)?;
    let limit = number(params, "limit", DEFAULT_PAGE, MAX_PAGE)?.max(1);
    let kind = params
        .get("kind")
        .map(String::as_str)
        .filter(|kind| !kind.is_empty());
    if let Some(kind) = kind
        && !KINDS.contains(&kind)
    {
        return Err(ExploreError::BadRequest(format!(
            "неизвестный тип узла: {kind}"
        )));
    }
    let group = params
        .get("group")
        .map(String::as_str)
        .filter(|group| !group.is_empty());
    let query = params.get("q").map(|q| q.trim()).filter(|q| !q.is_empty());
    if query.is_some_and(|q| q.chars().count() > 200) {
        return Err(ExploreError::BadRequest(
            "запрос длиннее 200 символов".to_owned(),
        ));
    }
    let sort = params.get("sort").map(String::as_str).unwrap_or("degree");
    if !["degree", "recent", "label"].contains(&sort) {
        return Err(ExploreError::BadRequest(format!(
            "неизвестная сортировка: {sort}"
        )));
    }

    let connection = open(database)?;
    let mut all = entries(&connection, space)?;
    let pairs = edge_pairs(&connection, space)?;
    let degrees = live_degrees(&pairs);
    for entry in &mut all {
        entry.degree = degrees.get(entry.id.as_str()).copied().unwrap_or(0);
    }
    let total_nodes = all.len();
    let by_id: HashMap<&str, &Entry> = all.iter().map(|entry| (entry.id.as_str(), entry)).collect();

    let keep = |entry: &Entry| {
        kind.is_none_or(|kind| entry.kind == kind)
            && group.is_none_or(|group| source_group(entry.source.as_deref()) == group)
    };
    let (mut selected, capped): (Vec<&Entry>, bool) = match query {
        Some(query) => {
            let (ids, capped) = matching_ids(&connection, space, query)?;
            (
                ids.iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .filter(|entry| keep(entry))
                    .collect(),
                capped,
            )
        }
        None => (all.iter().filter(|entry| keep(entry)).collect(), false),
    };
    // A search keeps its relevance order; browsing gets the requested one.
    if query.is_none() || sort != "degree" {
        match sort {
            "recent" => selected.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            }),
            "label" => selected.sort_by(|a, b| {
                a.label
                    .to_lowercase()
                    .cmp(&b.label.to_lowercase())
                    .then_with(|| a.id.cmp(&b.id))
            }),
            _ => selected.sort_by(|a, b| {
                b.degree
                    .cmp(&a.degree)
                    .then_with(|| b.salience.total_cmp(&a.salience))
                    .then_with(|| a.id.cmp(&b.id))
            }),
        }
    }
    let matched = selected.len();
    let rows = selected
        .iter()
        .skip(offset)
        .take(limit)
        .map(|entry| row_document(entry))
        .collect::<Vec<_>>();
    Ok(json!({
        "total_nodes": total_nodes,
        "matched": matched,
        "matches_capped": capped,
        "offset": offset,
        "limit": limit,
        "returned": rows.len(),
        "rows": rows,
    }))
}

/// One node in full, with a page of its neighbours.
///
/// # Errors
///
/// `NotFound` for an id with no current version in this space, `BadRequest`
/// for a missing id or bad paging, and the SQLite failure otherwise.
pub(crate) fn node(
    database: &Path,
    space: &str,
    params: &HashMap<String, String>,
) -> Result<Value, ExploreError> {
    // Imported ids are sentences turned into slugs; Cyrillic ones run past a
    // kilobyte, so the ceiling is generous and its failure says what it is.
    let id = params
        .get("id")
        .map(String::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ExploreError::BadRequest("нужен параметр id".to_owned()))?;
    if id.len() > MAX_ID_BYTES {
        return Err(ExploreError::BadRequest(format!(
            "id длиннее {MAX_ID_BYTES} байт"
        )));
    }
    let offset = number(params, "offset", 0, 1_000_000)?;
    let limit = number(params, "limit", DEFAULT_NEIGHBOURS, MAX_NEIGHBOURS)?.max(1);
    let by_links = match params.get("order").map(String::as_str) {
        None | Some("" | "relation") => false,
        Some("links") => true,
        Some(other) => {
            return Err(ExploreError::BadRequest(format!(
                "неизвестный порядок соседей: {other}"
            )));
        }
    };

    let connection = open(database)?;
    let found = connection.query_row(
        r#"SELECT v.label, v.kind, v.body, v.source, v.metadata, v.valid_from, v.recorded_at,
                  v.agent_id, v.revision, n.created_at, n.last_accessed, n.access_count,
                  n.salience, n.salience_layer, n.degree
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id AND v.superseded_at IS NULL
           WHERE n.space_id = ?1 AND n.id = ?2"#,
        params![space, id],
        |row| {
            let body: String = row.get(2)?;
            let metadata: String = row.get(4)?;
            let source: Option<String> = row.get(3)?;
            let body_chars = body.chars().count();
            Ok(json!({
                "id": id,
                "label": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
                "body": body.chars().take(BODY_EXCERPT_CHARS).collect::<String>(),
                "body_chars": body_chars,
                "body_truncated": body_chars > BODY_EXCERPT_CHARS,
                "group": source_group(source.as_deref()),
                "source": source,
                "metadata_keys": serde_json::from_str::<Value>(&metadata)
                    .ok()
                    .and_then(|value| value.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()))
                    .unwrap_or_default(),
                "valid_from": row.get::<_, i64>(5)?,
                "recorded_at": row.get::<_, i64>(6)?,
                "agent": row.get::<_, String>(7)?,
                "revision": row.get::<_, i64>(8)?,
                "created_at": row.get::<_, i64>(9)?,
                "last_accessed": row.get::<_, i64>(10)?,
                "access_count": row.get::<_, i64>(11)?,
                "salience": row.get::<_, f64>(12)?,
                "layer": row.get::<_, String>(13)?,
                "degree": row.get::<_, i64>(14)?,
            }))
        },
    );
    let mut document = match found {
        Ok(document) => document,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(ExploreError::NotFound(
                "такого узла нет в этом пространстве".to_owned(),
            ));
        }
        Err(error) => return Err(error.into()),
    };

    let mut statement = connection.prepare(
        r#"SELECT e.from_node_id, e.to_node_id, v.relation, v.confidence
           FROM edges AS e
           JOIN edge_versions AS v
             ON v.space_id = e.space_id AND v.edge_id = e.id AND v.superseded_at IS NULL
           WHERE e.space_id = ?1 AND (e.from_node_id = ?2 OR e.to_node_id = ?2)"#,
    )?;
    let links = statement
        .query_map(params![space, id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let pairs = edge_pairs(&connection, space)?;
    let degrees = live_degrees(&pairs);
    if let Some(object) = document.as_object_mut() {
        object.insert(
            "degree".to_owned(),
            json!(degrees.get(id).copied().unwrap_or(0)),
        );
    }
    let mut neighbour = connection.prepare(
        r#"SELECT v.label, v.kind, v.source, n.created_at FROM nodes AS n
           JOIN node_versions AS v
             ON v.node_id = n.id AND v.space_id = n.space_id AND v.superseded_at IS NULL
           WHERE n.space_id = ?1 AND n.id = ?2"#,
    )?;
    let mut described = Vec::with_capacity(links.len());
    for (from, to, relation, confidence) in &links {
        let outgoing = from == id;
        let other = if outgoing { to } else { from };
        let degree = degrees.get(other.as_str()).copied().unwrap_or(0);
        let (label, kind, source, created_at) = neighbour
            .query_row(params![space, other], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap_or_else(|_| {
                (
                    "(узел без текущей версии)".to_owned(),
                    String::new(),
                    None,
                    0,
                )
            });
        described.push((
            relation.clone(),
            degree,
            other.clone(),
            json!({
                "id": other,
                "label": shorten(&label, LABEL_CHARS),
                "kind": kind,
                "group": source_group(source.as_deref()),
                "created_at": created_at,
                "degree": degree,
                "relation": relation,
                "direction": if outgoing { "out" } else { "in" },
                "confidence": (confidence * 100.0).round() / 100.0,
            }),
        ));
    }
    if by_links {
        described.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.0.cmp(&b.0))
        });
    } else {
        described.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });
    }
    let total = described.len();
    let mut relation_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (relation, _, _, _) in &described {
        *relation_counts.entry(relation.clone()).or_default() += 1;
    }
    let page = described
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|(_, _, _, value)| value)
        .collect::<Vec<_>>();
    if let Some(object) = document.as_object_mut() {
        object.insert(
            "neighbours".to_owned(),
            json!({
                "total": total,
                "offset": offset,
                "limit": limit,
                "returned": page.len(),
                "by_relation": relation_counts,
                "rows": page,
            }),
        );
    }
    Ok(document)
}

/// How nodes are put into clusters, and whether that grouping is stored or
/// derived. Only the source group is the page's own construction.
fn cluster_key(by: &str, entry: &Entry) -> String {
    match by {
        "kind" => entry.kind.clone(),
        "month" => month_of(entry.created_at),
        _ => source_group(entry.source.as_deref()),
    }
}

fn cluster_meaning(by: &str) -> (&'static str, bool) {
    match by {
        "kind" => ("тип узла, записанный в базе", false),
        "month" => (
            "месяц, когда узел был записан в Brain (не время события)",
            false,
        ),
        _ => (
            "первая папка пути источника — производная группировка страницы, а не проекты, размеченные человеком",
            true,
        ),
    }
}

/// The whole space folded into clusters, with real edges summed between them.
///
/// This is the first picture: tens of piles rather than ten thousand dots,
/// every count exact, and the folding rule stated next to it.
///
/// # Errors
///
/// `BadRequest` for an unknown grouping, the SQLite failure otherwise.
pub(crate) fn clusters(
    database: &Path,
    space: &str,
    params: &HashMap<String, String>,
) -> Result<Value, ExploreError> {
    let by = params
        .get("by")
        .map(String::as_str)
        .filter(|by| !by.is_empty())
        .unwrap_or("group");
    if !["group", "kind", "month"].contains(&by) {
        return Err(ExploreError::BadRequest(format!(
            "неизвестная группировка: {by}"
        )));
    }
    let connection = open(database)?;
    let all = entries(&connection, space)?;
    let pairs = edge_pairs(&connection, space)?;
    let degrees = live_degrees(&pairs);

    let mut counts: HashMap<String, (usize, BTreeMap<String, usize>, i64, i64)> = HashMap::new();
    let mut key_of: HashMap<&str, String> = HashMap::with_capacity(all.len());
    for entry in &all {
        let key = cluster_key(by, entry);
        let slot = counts
            .entry(key.clone())
            .or_insert_with(|| (0, BTreeMap::new(), i64::MAX, i64::MIN));
        slot.0 += 1;
        *slot.1.entry(entry.kind.clone()).or_default() += 1;
        slot.2 = slot.2.min(entry.created_at);
        slot.3 = slot.3.max(entry.created_at);
        key_of.insert(entry.id.as_str(), key);
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    if by == "month" {
        ranked.sort_by(|a, b| a.0.cmp(&b.0));
    } else {
        ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
    }
    let cluster_total = ranked.len();
    // Months are few and ordered; source groups have a long tail of one-node
    // piles that would only be noise as separate dots.
    let listed = if by == "month" {
        ranked.len()
    } else {
        LISTED_CLUSTERS.min(ranked.len())
    };
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (index, (key, _)) in ranked.iter().take(listed).enumerate() {
        index_of.insert(key.as_str(), index);
    }
    let rest_index = (ranked.len() > listed).then_some(listed);

    let mut internal = vec![0_usize; listed + usize::from(rest_index.is_some())];
    let mut between: HashMap<(usize, usize), usize> = HashMap::new();
    let mut dangling = 0_usize;
    for (from, to) in &pairs {
        let (Some(a), Some(b)) = (key_of.get(from.as_str()), key_of.get(to.as_str())) else {
            dangling += 1;
            continue;
        };
        let a = index_of
            .get(a.as_str())
            .copied()
            .or(rest_index)
            .unwrap_or(0);
        let b = index_of
            .get(b.as_str())
            .copied()
            .or(rest_index)
            .unwrap_or(0);
        if a == b {
            internal[a] += 1;
        } else {
            *between.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }
    let mut cluster_links = between.into_iter().collect::<Vec<_>>();
    cluster_links.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let cluster_link_total = cluster_links.len();
    cluster_links.truncate(LISTED_CLUSTER_LINKS);

    let mut top_node: HashMap<&str, (&Entry, i64)> = HashMap::new();
    for entry in &all {
        let degree = degrees.get(entry.id.as_str()).copied().unwrap_or(0);
        let key = key_of[entry.id.as_str()].as_str();
        let candidate = (entry, degree);
        top_node
            .entry(key)
            .and_modify(|best| {
                if degree > best.1 || (degree == best.1 && entry.id < best.0.id) {
                    *best = candidate;
                }
            })
            .or_insert(candidate);
    }

    let mut rows = ranked
        .iter()
        .take(listed)
        .enumerate()
        .map(|(index, (key, (count, kinds, first, last)))| {
            let top = top_node.get(key.as_str());
            json!({
                "key": key,
                "count": count,
                "kinds": kinds,
                "internal_links": internal[index],
                "first_created_at": first,
                "last_created_at": last,
                "top_node": top.map(|(entry, degree)| json!({"id": entry.id, "label": shorten(&entry.label, DRAWN_LABEL_CHARS), "links": degree})),
            })
        })
        .collect::<Vec<_>>();
    if let Some(rest) = rest_index {
        let tail = &ranked[listed..];
        let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
        for (_, (_, tail_kinds, _, _)) in tail {
            for (kind, count) in tail_kinds {
                *kinds.entry(kind.clone()).or_default() += count;
            }
        }
        rows.push(json!({
            "key": null,
            "rest_clusters": tail.len(),
            "count": tail.iter().map(|(_, (count, ..))| count).sum::<usize>(),
            "kinds": kinds,
            "internal_links": internal[rest],
            "first_created_at": tail.iter().map(|(_, (_, _, first, _))| *first).min(),
            "last_created_at": tail.iter().map(|(_, (_, _, _, last))| *last).max(),
            "top_node": null,
        }));
    }
    let (meaning, derived) = cluster_meaning(by);
    Ok(json!({
        "space": space,
        "by": by,
        "derived": derived,
        "meaning": meaning,
        "total_nodes": all.len(),
        "total_edges": pairs.len(),
        "cluster_total": cluster_total,
        "clusters": rows,
        "cluster_links": cluster_links.into_iter().map(|((a, b), count)| json!([a, b, count])).collect::<Vec<_>>(),
        "cluster_link_total": cluster_link_total,
        "dangling_edges": dangling,
        "links_meaning": "число настоящих связей между узлами двух групп; сама линия между группами — сводка, а не ребро графа",
    }))
}

/// Exact edges among a set of nodes, as index pairs into `ids`.
///
/// One indexed lookup per node's outgoing edges, so the cost follows the
/// drawn set and its degree, not the size of the space.
fn edges_among(
    connection: &Connection,
    space: &str,
    ids: &[&str],
) -> rusqlite::Result<(Vec<Value>, Vec<String>, usize)> {
    let position: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect();
    let mut statement = connection.prepare(
        // One row per logical edge even if a crash ever left two current
        // versions: the newest revision names the relation.
        r#"SELECT e.to_node_id, v.relation FROM edges AS e
           JOIN edge_versions AS v ON v.row_id = (
               SELECT c.row_id FROM edge_versions AS c
               WHERE c.space_id = e.space_id AND c.edge_id = e.id AND c.superseded_at IS NULL
               ORDER BY c.revision DESC LIMIT 1)
           WHERE e.space_id = ?1 AND e.from_node_id = ?2
           ORDER BY e.id"#,
    )?;
    let mut relations: Vec<String> = Vec::new();
    let mut relation_index: HashMap<String, usize> = HashMap::new();
    let mut edges = Vec::new();
    let mut total = 0_usize;
    for (from_index, from) in ids.iter().enumerate() {
        let rows = statement.query_map(params![space, from], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (to, relation) = row?;
            let Some(&to_index) = position.get(to.as_str()) else {
                continue;
            };
            total += 1;
            if edges.len() >= MAX_DRAWN_EDGES {
                continue;
            }
            let next = relations.len();
            let relation_at = *relation_index.entry(relation.clone()).or_insert_with(|| {
                relations.push(relation);
                next
            });
            edges.push(json!([from_index, to_index, relation_at]));
        }
    }
    Ok((edges, relations, total))
}

/// A bounded set of nodes to draw, with the exact edges among them.
///
/// Without a query the set is the most linked nodes matching the filters;
/// with one it is the best matches. The answer says how many matched and how
/// many edges exist among the drawn nodes, so the picture never passes for the
/// whole graph.
///
/// # Errors
///
/// `BadRequest` for a bad filter or limit; the SQLite failure otherwise.
pub(crate) fn graph(
    database: &Path,
    space: &str,
    params: &HashMap<String, String>,
) -> Result<Value, ExploreError> {
    let limit = number(params, "limit", DEFAULT_DRAWN, MAX_DRAWN)?.max(1);
    let kind = params
        .get("kind")
        .map(String::as_str)
        .filter(|kind| !kind.is_empty());
    if let Some(kind) = kind
        && !KINDS.contains(&kind)
    {
        return Err(ExploreError::BadRequest(format!(
            "неизвестный тип узла: {kind}"
        )));
    }
    let group = params
        .get("group")
        .map(String::as_str)
        .filter(|group| !group.is_empty());
    let month = params
        .get("month")
        .map(String::as_str)
        .filter(|month| !month.is_empty());
    let query = params.get("q").map(|q| q.trim()).filter(|q| !q.is_empty());
    if query.is_some_and(|q| q.chars().count() > 200) {
        return Err(ExploreError::BadRequest(
            "запрос длиннее 200 символов".to_owned(),
        ));
    }

    let connection = open(database)?;
    let all = entries(&connection, space)?;
    let pairs = edge_pairs(&connection, space)?;
    let degrees = live_degrees(&pairs);
    let degree = |entry: &Entry| degrees.get(entry.id.as_str()).copied().unwrap_or(0);
    let keep = |entry: &Entry| {
        kind.is_none_or(|kind| entry.kind == kind)
            && group.is_none_or(|group| source_group(entry.source.as_deref()) == group)
            && month.is_none_or(|month| month_of(entry.created_at) == month)
    };
    let (selected, capped): (Vec<&Entry>, bool) = match query {
        Some(query) => {
            let by_id: HashMap<&str, &Entry> =
                all.iter().map(|entry| (entry.id.as_str(), entry)).collect();
            let (ids, capped) = matching_ids(&connection, space, query)?;
            (
                ids.iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .filter(|entry| keep(entry))
                    .collect(),
                capped,
            )
        }
        None => {
            let mut selected = all.iter().filter(|entry| keep(entry)).collect::<Vec<_>>();
            selected.sort_by(|a, b| {
                degree(b)
                    .cmp(&degree(a))
                    .then_with(|| b.created_at.cmp(&a.created_at))
                    .then_with(|| a.id.cmp(&b.id))
            });
            (selected, false)
        }
    };
    let matched = selected.len();
    let drawn = &selected[..matched.min(limit)];
    let ids = drawn
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>();
    let (edges, relations, among) = edges_among(&connection, space, &ids)?;
    Ok(json!({
        "space": space,
        "selection": {
            "kind": kind, "group": group, "month": month, "q": query,
            "order": if query.is_some() { "relevance" } else { "links" },
        },
        "total_nodes": all.len(),
        "total_edges": pairs.len(),
        "matched": matched,
        "matches_capped": capped,
        "limit": limit,
        "returned": drawn.len(),
        "nodes": drawn.iter().map(|entry| drawn_node(entry, degree(entry))).collect::<Vec<_>>(),
        "relations": relations,
        "edges": edges,
        "edges_among": among,
        "edges_truncated": among > MAX_DRAWN_EDGES,
    }))
}

fn drawn_node(entry: &Entry, degree: i64) -> Value {
    json!({
        "id": entry.id,
        "label": shorten(&entry.label, DRAWN_LABEL_CHARS),
        "kind": entry.kind,
        "group": source_group(entry.source.as_deref()),
        "links": degree,
        "created_at": entry.created_at,
    })
}

/// Exact edges among nodes the page already holds, from a JSON body
/// `{"ids": [...]}`. IDs not in this space are named back so the page can drop
/// them instead of drawing a node another space owns.
///
/// # Errors
///
/// `BadRequest` for a malformed body, too many or overlong IDs; the SQLite
/// failure otherwise.
pub(crate) fn links(database: &Path, space: &str, body: &[u8]) -> Result<Value, ExploreError> {
    let parsed: Value = serde_json::from_slice(body)
        .map_err(|_| ExploreError::BadRequest("тело запроса — не JSON".to_owned()))?;
    let Some(list) = parsed.get("ids").and_then(Value::as_array) else {
        return Err(ExploreError::BadRequest("нужен массив ids".to_owned()));
    };
    if list.len() > MAX_DRAWN {
        return Err(ExploreError::BadRequest(format!(
            "не больше {MAX_DRAWN} id за запрос"
        )));
    }
    let mut ids: Vec<&str> = Vec::with_capacity(list.len());
    let mut seen = HashSet::with_capacity(list.len());
    for value in list {
        let id = value
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
            .ok_or_else(|| {
                ExploreError::BadRequest(format!("id — непустая строка до {MAX_ID_BYTES} байт"))
            })?;
        // Answers index into the page's own array, so a repeat cannot be
        // dropped silently without shifting every index after it.
        if !seen.insert(id) {
            return Err(ExploreError::BadRequest("id повторяется".to_owned()));
        }
        ids.push(id);
    }
    let connection = open(database)?;
    let mut present =
        connection.prepare("SELECT EXISTS(SELECT 1 FROM nodes WHERE space_id = ?1 AND id = ?2)")?;
    let mut missing = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        if !present.query_row(params![space, id], |row| row.get::<_, bool>(0))? {
            missing.push(index);
        }
    }
    let (edges, relations, among) = edges_among(&connection, space, &ids)?;
    Ok(json!({
        "space": space,
        "count": ids.len(),
        "missing": missing,
        "relations": relations,
        "edges": edges,
        "edges_among": among,
        "edges_truncated": among > MAX_DRAWN_EDGES,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A database with `count` nodes in a chain, built through the real schema.
    pub(crate) struct Scratch(pub(crate) std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A database with `count` nodes in a chain, built through the real schema.
    pub(crate) fn chain_database(count: usize) -> (Scratch, std::path::PathBuf) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "brain-explore-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
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
        for index in 0..count {
            let kind = KINDS[index % KINDS.len()];
            let source = if index % 2 == 0 {
                format!("/home/someone/projects/alpha/file{index}.rs")
            } else {
                format!("docs/notes/{index}.md")
            };
            connection
                .execute(
                    "INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id,
                                       created_at, last_accessed, degree)
                     VALUES (?1, 'default', 't', 's', ?2, ?2, ?3)",
                    params![
                        format!("n{index}"),
                        1_700_000_000 + index as i64,
                        (count - index) as i64
                    ],
                )
                .expect("node");
            connection
                .execute(
                    "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, source,
                                               valid_from, recorded_at, agent_id, session_id)
                     VALUES (?1, 'default', 1, ?2, ?3, ?4, ?5, 1, 1, 't', 's')",
                    params![
                        format!("n{index}"),
                        kind,
                        format!("node number {index}"),
                        if index == count - 1 { "the hidden zebra fact".to_owned() } else { format!("body {index}") },
                        source,
                    ],
                )
                .expect("version");
            if index > 0 {
                connection
                    .execute(
                        "INSERT INTO edges(id, space_id, from_node_id, to_node_id, created_by_agent_id,
                                           created_in_session_id, created_at, last_accessed)
                         VALUES (?1, 'default', ?2, ?3, 't', 's', 1, 1)",
                        params![format!("e{index}"), format!("n{}", index - 1), format!("n{index}")],
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
        }
        (Scratch(directory), path)
    }

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_overview_counts_every_node_not_a_sample() {
        let (_scratch, path) = chain_database(30);
        let overview = overview(&path, "default").expect("overview");
        assert_eq!(overview["total_nodes"], 30);
        assert_eq!(overview["total_edges"], 29);
        let kinds = overview["kinds"].as_object().expect("kinds");
        assert_eq!(kinds.values().map(|v| v.as_u64().unwrap()).sum::<u64>(), 30);
        let grouped = overview["groups"].as_array().expect("groups");
        assert!(grouped.iter().any(|g| g["group"] == "projects"));
        assert!(grouped.iter().any(|g| g["group"] == "docs"));
        assert_eq!(overview["scanned_nodes"], 30);
        assert_eq!(overview["truncated"], false);
    }

    #[test]
    fn the_overview_bounds_buckets_and_reports_it() {
        let (_scratch, path) = chain_database(3);
        let mut kinds = BTreeMap::new();
        let mut overflow = 0;
        for index in 0..MAX_OVERVIEW_BUCKETS + 5 {
            bump(&mut kinds, format!("k{index}"), &mut overflow);
        }
        bump(&mut kinds, "k0".to_owned(), &mut overflow);
        assert_eq!(kinds.len(), MAX_OVERVIEW_BUCKETS);
        assert_eq!(overflow, 5);
        assert_eq!(kinds["k0"], 2);
        let overview = overview(&path, "default").expect("overview");
        assert_eq!(overview["total_nodes"], 3);
        assert_eq!(overview["truncation"]["unbucketed_kinds"], 0);
        assert_eq!(overview["truncated"], false);
    }

    #[test]
    fn paging_reaches_the_last_node() {
        let (_scratch, path) = chain_database(45);
        let page = nodes(
            &path,
            "default",
            &query(&[("offset", "40"), ("limit", "10")]),
        )
        .expect("page");
        assert_eq!(page["matched"], 45);
        assert_eq!(page["returned"], 5);
        // Sorted by degree descending; the chain's last node has the lowest.
        assert_eq!(page["rows"][4]["id"], "n44");
    }

    #[test]
    fn search_finds_a_node_by_body_text_and_by_label_fragment() {
        let (_scratch, path) = chain_database(20);
        let page = nodes(&path, "default", &query(&[("q", "zebra")])).expect("search");
        assert_eq!(page["matched"], 1);
        assert_eq!(page["rows"][0]["id"], "n19");
        let page = nodes(&path, "default", &query(&[("q", "number 7")])).expect("search");
        assert!(
            page["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["id"] == "n7")
        );
    }

    #[test]
    fn filters_combine_and_bad_input_is_refused() {
        let (_scratch, path) = chain_database(32);
        let page = nodes(
            &path,
            "default",
            &query(&[("kind", "memory"), ("group", "docs")]),
        )
        .expect("filter");
        for row in page["rows"].as_array().unwrap() {
            assert_eq!(row["kind"], "memory");
            assert_eq!(row["group"], "docs");
        }
        assert!(matches!(
            nodes(&path, "default", &query(&[("kind", "secret")])),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            nodes(&path, "default", &query(&[("limit", "100000")])),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            nodes(&path, "default", &query(&[("offset", "-1")])),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            nodes(&path, "default", &query(&[("sort", "drop table")])),
            Err(ExploreError::BadRequest(_))
        ));
        let quoted =
            nodes(&path, "default", &query(&[("q", "\" OR * NEAR(")])).expect("odd query is data");
        assert_eq!(quoted["matched"], 0);
    }

    #[test]
    fn a_node_comes_with_paged_neighbours() {
        let (_scratch, path) = chain_database(10);
        let detail = node(&path, "default", &query(&[("id", "n5")])).expect("node");
        assert_eq!(detail["label"], "node number 5");
        assert_eq!(detail["neighbours"]["total"], 2);
        let directions = detail["neighbours"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["direction"].as_str().unwrap().to_owned())
            .collect::<HashSet<_>>();
        assert_eq!(
            directions,
            HashSet::from(["in".to_owned(), "out".to_owned()])
        );
        let paged = node(
            &path,
            "default",
            &query(&[("id", "n5"), ("offset", "1"), ("limit", "1")]),
        )
        .expect("node");
        assert_eq!(paged["neighbours"]["returned"], 1);
        assert!(matches!(
            node(&path, "default", &query(&[("id", "missing")])),
            Err(ExploreError::NotFound(_))
        ));
        assert!(matches!(
            node(&path, "default", &query(&[])),
            Err(ExploreError::BadRequest(_))
        ));
    }

    #[test]
    fn spaces_are_chosen_from_the_stored_ones_and_never_mixed() {
        let (_scratch, path) = chain_database(6);
        let connection = Connection::open(&path).expect("open");
        connection
            .execute_batch(
                "INSERT INTO spaces(id, kind, label, created_by_agent_id, created_at)
                   VALUES ('other', 'project', 'other', 't', 1);
                 INSERT INTO sessions(id, space_id, agent_id, started_at) VALUES ('s2', 'other', 't', 1);
                 INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id, created_at, last_accessed)
                   VALUES ('n0', 'other', 't', 's2', 5, 5);
                 INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, valid_from, recorded_at, agent_id, session_id)
                   VALUES ('n0', 'other', 1, 'memory', 'other space secret', 'only here', 1, 1, 't', 's2');",
            )
            .expect("second space");
        drop(connection);
        let space = |value: &str| query(&[("space", value)]);
        assert_eq!(
            resolve_space(&path, &query(&[]), "default").unwrap(),
            "default"
        );
        assert_eq!(
            resolve_space(&path, &space("other"), "default").unwrap(),
            "other"
        );
        assert!(matches!(
            resolve_space(&path, &space("nope"), "default"),
            Err(ExploreError::BadRequest(_))
        ));
        let other = overview(&path, "other").expect("overview");
        assert_eq!(other["total_nodes"], 1);
        let listed = other["spaces"].as_array().unwrap();
        assert!(
            listed
                .iter()
                .any(|s| s["space"] == "default" && s["nodes"] == 6)
        );
        // The same id in two spaces resolves to each space's own node.
        assert_eq!(
            node(&path, "other", &query(&[("id", "n0")])).unwrap()["label"],
            "other space secret"
        );
        assert_eq!(
            node(&path, "default", &query(&[("id", "n0")])).unwrap()["label"],
            "node number 0"
        );
        let found = nodes(&path, "default", &query(&[("q", "secret")])).unwrap();
        assert_eq!(
            found["matched"], 0,
            "a search must not reach into another space"
        );
    }

    #[test]
    fn a_long_cyrillic_id_opens() {
        let (_scratch, path) = chain_database(2);
        let long = format!("g:{}", "решение_".repeat(120));
        assert!(long.len() > 1_000);
        let connection = Connection::open(&path).expect("open");
        connection
            .execute(
                "INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id, created_at, last_accessed)
                 VALUES (?1, 'default', 't', 's', 1, 1)",
                [&long],
            )
            .expect("node");
        connection
            .execute(
                "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, valid_from, recorded_at, agent_id, session_id)
                 VALUES (?1, 'default', 1, 'decision', 'long one', 'b', 1, 1, 't', 's')",
                [&long],
            )
            .expect("version");
        drop(connection);
        assert_eq!(
            node(&path, "default", &query(&[("id", long.as_str())])).unwrap()["label"],
            "long one"
        );
        let huge = "x".repeat(MAX_ID_BYTES + 1);
        assert!(matches!(
            node(&path, "default", &query(&[("id", huge.as_str())])),
            Err(ExploreError::BadRequest(_))
        ));
    }

    #[test]
    fn source_groups_are_short_and_path_free_of_the_home_owner() {
        assert_eq!(
            source_group(Some("/home/alice/notes-hub/memory/MEMORY.md")),
            "notes-hub"
        );
        assert_eq!(
            source_group(Some("/home/alice/.codex/sessions/2026/x.jsonl")),
            ".codex/sessions"
        );
        assert_eq!(source_group(Some("docs/audits/x.md")), "docs");
        assert_eq!(source_group(Some("graphify")), "graphify");
        assert_eq!(source_group(None), "(без источника)");
        assert_eq!(source_group(Some("  ")), "(без источника)");
        assert_eq!(source_group(Some("/run/media/x/USBDRIVE/a")), "/run/media");
    }

    /// One more node (and optionally an edge from `link_from`) in a test space.
    fn add_node(
        path: &Path,
        space: &str,
        id: &str,
        source: &str,
        created_at: i64,
        link_from: Option<&str>,
    ) {
        let connection = Connection::open(path).expect("open");
        connection
            .execute(
                "INSERT INTO nodes(id, space_id, created_by_agent_id, created_in_session_id, created_at, last_accessed)
                 VALUES (?1, ?2, 't', 's', ?3, ?3)",
                params![id, space, created_at],
            )
            .expect("node");
        connection
            .execute(
                "INSERT INTO node_versions(node_id, space_id, revision, kind, label, body, source, valid_from, recorded_at, agent_id, session_id)
                 VALUES (?1, ?2, 1, 'concept', ?1, 'b', ?3, 1, 1, 't', 's')",
                params![id, space, source],
            )
            .expect("version");
        if let Some(from) = link_from {
            let edge = format!("e-{from}-{id}");
            connection
                .execute(
                    "INSERT INTO edges(id, space_id, from_node_id, to_node_id, created_by_agent_id, created_in_session_id, created_at, last_accessed)
                     VALUES (?1, ?2, ?3, ?4, 't', 's', 1, 1)",
                    params![edge, space, from, id],
                )
                .expect("edge");
            connection
                .execute(
                    "INSERT INTO edge_versions(edge_id, space_id, revision, relation, weight, confidence, valid_from, recorded_at, agent_id, session_id)
                     VALUES (?1, ?2, 1, 'calls', 0.5, 0.9, 1, 1, 't', 's')",
                    params![edge, space],
                )
                .expect("edge version");
        }
    }

    #[test]
    fn links_are_counted_from_edges_not_the_stale_degree_column() {
        // The fixture stores degree = count - index; real chain degree is 1 or 2.
        let (_scratch, path) = chain_database(12);
        let page = nodes(&path, "default", &query(&[("q", "number 0")])).expect("search");
        let row = page["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "n0")
            .expect("n0");
        assert_eq!(row["degree"], 1);
        let detail = node(&path, "default", &query(&[("id", "n5")])).expect("node");
        assert_eq!(detail["degree"], 2);
        for neighbour in detail["neighbours"]["rows"].as_array().unwrap() {
            assert_eq!(neighbour["degree"], 2);
            assert!(neighbour["group"].is_string() && neighbour["created_at"].is_i64());
        }
        assert!(matches!(
            node(&path, "default", &query(&[("id", "n5"), ("order", "x")])),
            Err(ExploreError::BadRequest(_))
        ));
    }

    #[test]
    fn clusters_fold_every_node_and_sum_only_real_edges() {
        let (_scratch, path) = chain_database(30);
        let by_group = clusters(&path, "default", &query(&[])).expect("clusters");
        assert_eq!(by_group["derived"], true);
        let rows = by_group["clusters"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|c| c["count"].as_u64().unwrap())
                .sum::<u64>(),
            30
        );
        // The chain alternates projects/docs, so every edge runs between the two.
        assert_eq!(by_group["cluster_links"], json!([[0, 1, 29]]));
        assert_eq!(rows[0]["internal_links"], 0);
        let by_kind = clusters(&path, "default", &query(&[("by", "kind")])).expect("kinds");
        assert_eq!(by_kind["derived"], false);
        assert_eq!(by_kind["clusters"].as_array().unwrap().len(), KINDS.len());
        assert!(matches!(
            clusters(&path, "default", &query(&[("by", "mood")])),
            Err(ExploreError::BadRequest(_))
        ));

        for index in 0..60 {
            add_node(
                &path,
                "default",
                &format!("g{index}"),
                &format!("tool{index}"),
                5,
                None,
            );
        }
        let many = clusters(&path, "default", &query(&[])).expect("many groups");
        let rows = many["clusters"].as_array().unwrap();
        assert_eq!(rows.len(), LISTED_CLUSTERS + 1);
        let rest = rows.last().unwrap();
        assert!(rest["key"].is_null());
        assert_eq!(many["cluster_total"].as_u64().unwrap() as usize, 62);
        assert_eq!(
            rows.iter()
                .map(|c| c["count"].as_u64().unwrap())
                .sum::<u64>(),
            90,
            "the rest is summed, not dropped"
        );
    }

    #[test]
    fn a_drawn_selection_is_the_most_linked_with_exact_edges_and_honest_counts() {
        let (_scratch, path) = chain_database(50);
        add_node(&path, "default", "hub", "hub/x", 1, None);
        for index in 0..10 {
            // hub -> n0..n9 via fresh edges
            let connection = Connection::open(&path).expect("open");
            let edge = format!("h{index}");
            connection.execute(
                "INSERT INTO edges(id, space_id, from_node_id, to_node_id, created_by_agent_id, created_in_session_id, created_at, last_accessed)
                 VALUES (?1, 'default', 'hub', ?2, 't', 's', 1, 1)", params![edge, format!("n{index}")]).expect("edge");
            connection.execute(
                "INSERT INTO edge_versions(edge_id, space_id, revision, relation, weight, confidence, valid_from, recorded_at, agent_id, session_id)
                 VALUES (?1, 'default', 1, 'calls', 0.5, 0.9, 1, 1, 't', 's')", params![edge]).expect("edge version");
        }
        let drawn = graph(&path, "default", &query(&[("limit", "5")])).expect("graph");
        assert_eq!(drawn["matched"], 51);
        assert_eq!(drawn["returned"], 5);
        let ids = drawn["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["hub", "n9", "n8", "n7", "n6"]);
        assert_eq!(drawn["nodes"][0]["links"], 10);
        // hub-n6..n9 and the chain n6-n7-n8-n9: seven, none to undrawn nodes.
        assert_eq!(drawn["edges_among"], 7);
        assert_eq!(drawn["edges"].as_array().unwrap().len(), 7);
        for edge in drawn["edges"].as_array().unwrap() {
            assert!(edge[0].as_u64().unwrap() < 5 && edge[1].as_u64().unwrap() < 5);
        }
        let grouped = graph(&path, "default", &query(&[("group", "hub")])).expect("group");
        assert_eq!(grouped["returned"], 1);
        let searched = graph(&path, "default", &query(&[("q", "zebra")])).expect("search");
        assert_eq!(searched["nodes"][0]["id"], "n49");
        assert_eq!(searched["selection"]["order"], "relevance");
        assert!(matches!(
            graph(&path, "default", &query(&[("limit", "1001")])),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            graph(&path, "default", &query(&[("kind", "secret")])),
            Err(ExploreError::BadRequest(_))
        ));
    }

    #[test]
    fn links_answer_among_the_given_ids_in_their_space_only() {
        let (_scratch, path) = chain_database(10);
        let body = br#"{"ids":["n1","n2","n3","not-here"]}"#;
        let answer = links(&path, "default", body).expect("links");
        assert_eq!(answer["edges_among"], 2);
        assert_eq!(answer["edges"], json!([[0, 1, 0], [1, 2, 0]]));
        assert_eq!(answer["missing"], json!([3]));
        // Two current versions of one edge (a crash leftover) still draw one line.
        Connection::open(&path)
            .expect("open")
            .execute(
                "INSERT INTO edge_versions(edge_id, space_id, revision, relation, weight, confidence, valid_from, recorded_at, agent_id, session_id)
                 VALUES ('e2', 'default', 2, 'calls', 0.5, 0.9, 1, 1, 't', 's')",
                [],
            )
            .expect("duplicate current version");
        let deduped = links(&path, "default", body).expect("links");
        assert_eq!(deduped["edges_among"], 2);
        assert_eq!(
            deduped["edges"][1],
            json!([1, 2, 1]),
            "newest revision names the relation"
        );

        let connection = Connection::open(&path).expect("open");
        connection
            .execute_batch(
                "INSERT INTO spaces(id, kind, label, created_by_agent_id, created_at) VALUES ('other', 'project', 'other', 't', 1);",
            )
            .expect("space");
        drop(connection);
        let foreign = links(&path, "other", body).expect("other space");
        assert_eq!(
            foreign["missing"],
            json!([0, 1, 2, 3]),
            "ids from another space are never matched"
        );
        assert_eq!(foreign["edges_among"], 0);

        assert!(matches!(
            links(&path, "default", b"not json"),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            links(&path, "default", br#"{"ids":["n1","n1"]}"#),
            Err(ExploreError::BadRequest(_))
        ));
        assert!(matches!(
            links(&path, "default", br#"{"ids":[""]}"#),
            Err(ExploreError::BadRequest(_))
        ));
        let too_many = json!({"ids": (0..=MAX_DRAWN).map(|i| format!("x{i}")).collect::<Vec<_>>()})
            .to_string();
        assert!(matches!(
            links(&path, "default", too_many.as_bytes()),
            Err(ExploreError::BadRequest(_))
        ));
    }

    #[test]
    fn query_strings_decode() {
        let parsed = parse_query("q=%D0%BC%D0%BE%D0%B7%D0%B3+x&kind=code&bad=%zz&tail=%4");
        assert_eq!(parsed["q"], "мозг x");
        assert_eq!(parsed["kind"], "code");
        assert_eq!(parsed["bad"], "%zz");
        assert_eq!(parsed["tail"], "%4");
    }

    #[test]
    fn months_are_civil() {
        assert_eq!(month_of(0), "1970-01");
        assert_eq!(month_of(1_791_417_600), "2026-10");
    }
}
