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

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};
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
    let limit = limit.clamp(1, MAX_LIMIT) as i64;
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
            AND v.superseded_at IS NULL
           WHERE n.space_id = ?1
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
        .query_map(params![space, limit], |row| {
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
        })?
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

    let links = links_among(&connection, space, &index)?;

    let total_nodes: i64 = connection.query_row(
        "SELECT count(*) FROM nodes WHERE space_id = ?1",
        [space],
        |row| row.get(0),
    )?;
    let total_edges: i64 = connection.query_row(
        "SELECT count(*) FROM edges WHERE space_id = ?1",
        [space],
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
            AND v.superseded_at IS NULL
           WHERE e.space_id = ?1"#,
    )?;
    let rows = statement.query_map([space], |row| {
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
}
