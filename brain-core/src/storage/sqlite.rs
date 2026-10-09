//! SQLite-backed implementation of the storage boundary.

#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, params, params_from_iter,
};

use super::migrations::{MIGRATIONS, SCHEMA_VERSION};
use crate::salience::{DAY, SalienceInput, layer_of, next_recalculation_at, salience};
use crate::{
    Agent, AsOf, Edge, GraphNeighbor, MaintenanceReport, MaterializedPath, NewEdge, NewEmbedding,
    NewNode, Node, NodeKind, NodeReplacement, SalienceLayer, SearchHit, SearchLayers, Session,
    Space, StorageError, StorageResult, StorageStatus, Timestamp, VectorSearchHit,
};

const MAX_INLINE_DEGREE: i64 = 1_000;
const VACUUM_INTERVAL: Timestamp = 7 * DAY;

const NODE_AS_OF_SQL: &str = r#"
SELECT n.id, v.row_id, v.revision, v.space_id, v.kind, v.label, v.body,
       v.source, v.metadata, v.valid_from, v.valid_to, v.recorded_at,
       v.superseded_at, v.agent_id, v.session_id, n.last_accessed,
       n.access_count, n.salience, n.created_at, n.source_weight, n.degree,
       n.salience_layer, n.token_len, n.next_salience_at
FROM node_versions AS v
JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
WHERE v.space_id = ?1
  AND v.node_id = ?2
  AND v.valid_from <= ?3
  AND (v.valid_to IS NULL OR ?3 < v.valid_to)
  AND v.recorded_at <= ?4
  AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
ORDER BY v.recorded_at DESC, v.revision DESC
LIMIT 1
"#;

const EDGE_AS_OF_SQL: &str = r#"
SELECT e.id, v.row_id, v.revision, v.space_id, e.from_node_id, e.to_node_id,
       v.relation, v.weight, v.confidence, v.metadata, v.valid_from, v.valid_to,
       v.recorded_at, v.superseded_at, v.agent_id, v.session_id,
       e.last_accessed, e.access_count, e.salience
FROM edge_versions AS v
JOIN edges AS e ON e.id = v.edge_id AND e.space_id = v.space_id
WHERE v.space_id = ?1
  AND v.edge_id = ?2
  AND v.valid_from <= ?3
  AND (v.valid_to IS NULL OR ?3 < v.valid_to)
  AND v.recorded_at <= ?4
  AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
ORDER BY v.recorded_at DESC, v.revision DESC
LIMIT 1
"#;

/// Operations accepted by the single writer. Each variant is one savepoint.
pub(super) enum WriteOperation {
    CreateAgent(Agent),
    CreateSpace(Space),
    CreateSession(Session),
    EnsureContext {
        agent: Box<Agent>,
        space: Box<Space>,
        session: Box<Session>,
    },
    InsertNodes(Vec<NewNode>),
    ReplaceNode {
        space_id: String,
        node_id: String,
        replacement: NodeReplacement,
    },
    InsertEdges {
        edges: Vec<NewEdge>,
        bulk: bool,
    },
    UpsertEmbeddings(Vec<NewEmbedding>),
    RecordNodeAccess {
        space_id: String,
        node_id: String,
        now: Timestamp,
    },
    RecordNodeAccesses {
        space_id: String,
        node_ids: Vec<String>,
        now: Timestamp,
    },
    ForgetMemory {
        space_id: String,
        node_id: String,
        agent_id: String,
        session_id: String,
        valid_to: Timestamp,
        recorded_at: Timestamp,
    },
    Noop,
}

/// SQLite connection owned exclusively by the storage actor.
/// Creates a missing database file owner-only before SQLite opens it, so the
/// memory store is 0600 whatever the umask; SQLite gives its `-wal`, `-shm`
/// and journal files the main file's mode. An existing file (or a symlink, or
/// an in-memory/URI name) is left untouched; failure here is not fatal because
/// SQLite reports any real problem opening the path itself.
fn create_private_database_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let name = path.as_os_str();
        if name.is_empty() || name == ":memory:" || name.to_string_lossy().starts_with("file:") {
            return;
        }
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path);
    }
    #[cfg(not(unix))]
    let _ = path;
}

pub(super) struct SqliteDatabase {
    connection: Connection,
}

impl SqliteDatabase {
    pub(super) fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        register_sqlite_vec()?;
        create_private_database_file(path.as_ref());
        Self::from_connection(Connection::open(path)?)
    }

    pub(super) fn open_in_memory() -> StorageResult<Self> {
        register_sqlite_vec()?;
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Opens an existing, fully migrated database for reading only: no file
    /// is created, no pragma that writes the header runs, no migration is
    /// applied, and `query_only` makes any stray write an error.
    pub(super) fn open_read_only(path: impl AsRef<Path>) -> StorageResult<Self> {
        let path = path.as_ref();
        if !path.is_file() {
            return Err(StorageError::NotFound(format!(
                "database {} does not exist",
                path.display()
            )));
        }
        register_sqlite_vec()?;
        // Normal SQLite read-only locking, never `immutable`: another process
        // may start writing and checkpointing at any time. For a WAL database
        // SQLite may create or use its `-wal`/`-shm` coordination files; that
        // is lock bookkeeping, not application data, and is allowed.
        let unavailable = |error: rusqlite::Error| {
            StorageError::InvalidData(format!(
                "database {} cannot be read safely read-only: {error}; SQLite needs its WAL \
                 coordination files (-wal/-shm) beside the database, so a read-only directory \
                 without them is refused rather than read without locks",
                path.display()
            ))
        };
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(unavailable)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(unavailable)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(unavailable)?;
        verify_current_schema(&connection).map_err(|error| match error {
            StorageError::Sqlite(error) => unavailable(error),
            other => other,
        })?;
        Ok(Self { connection })
    }

    fn from_connection(mut connection: Connection) -> StorageResult<Self> {
        configure_connection(&connection)?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }
}

impl SqliteDatabase {
    pub(super) fn schema_version(&self) -> StorageResult<u32> {
        read_schema_version(&self.connection)
    }

    pub(super) fn apply_write_batch(
        &mut self,
        operations: Vec<WriteOperation>,
    ) -> Vec<StorageResult<()>> {
        let operation_count = operations.len();
        let mut transaction = match self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(transaction) => transaction,
            Err(error) => {
                let message = format!("could not start writer transaction: {error}");
                return actor_errors(operation_count, &message);
            }
        };

        let mut results = Vec::with_capacity(operation_count);
        for operation in operations {
            let result = match transaction.savepoint() {
                Ok(savepoint) => {
                    let result = apply_write_operation(&savepoint, operation);
                    if result.is_ok() {
                        savepoint.commit().map_err(StorageError::from)
                    } else {
                        result
                    }
                }
                Err(error) => Err(StorageError::from(error)),
            };
            results.push(result);
        }

        if let Err(error) = transaction.commit() {
            return actor_errors(
                operation_count,
                &format!("could not commit writer batch: {error}"),
            );
        }
        results
    }

    pub(super) fn node_as_of(
        &self,
        space_id: &str,
        node_id: &str,
        as_of: AsOf,
    ) -> StorageResult<Option<Node>> {
        let stored = self
            .connection
            .query_row(
                NODE_AS_OF_SQL,
                params![space_id, node_id, as_of.valid_at, as_of.recorded_at],
                stored_node_from_row,
            )
            .optional()?;
        stored.map(StoredNode::into_node).transpose()
    }

    pub(super) fn list_memories(
        &self,
        space_id: &str,
        created_by_agent_id: Option<&str>,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<Node>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            r#"SELECT n.id, v.row_id, v.revision, v.space_id, v.kind, v.label,
                      v.body, v.source, v.metadata, v.valid_from, v.valid_to,
                      v.recorded_at, v.superseded_at, v.agent_id, v.session_id,
                      n.last_accessed, n.access_count, n.salience, n.created_at,
                      n.source_weight, n.degree, n.salience_layer, n.token_len,
                      n.next_salience_at
               FROM node_versions AS v
               JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
               WHERE v.space_id = ?1
                 AND json_extract(v.metadata, '$.brain_memory') = 1
                 AND (?2 IS NULL OR n.created_by_agent_id = ?2)
                 AND v.valid_from <= ?3
                 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
                 AND v.recorded_at <= ?4
                 AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
               ORDER BY v.recorded_at DESC, v.row_id DESC
               LIMIT ?5"#,
        )?;
        let rows = statement.query_map(
            params![
                space_id,
                created_by_agent_id,
                as_of.valid_at,
                as_of.recorded_at,
                sqlite_limit(limit)?
            ],
            stored_node_from_row,
        )?;
        let mut nodes = Vec::new();
        for row in rows {
            nodes.push(row?.into_node()?);
        }
        Ok(nodes)
    }

    pub(super) fn search_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>> {
        self.search_nodes_in_layers(space_id, query, as_of, SearchLayers::ALL, limit)
    }

    pub(super) fn search_nodes_in_layers(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>> {
        if limit == 0 || !any_layer(layers) {
            return Ok(Vec::new());
        }
        let candidate_limit = sqlite_limit(limit.saturating_mul(4).min(4_096))?;
        let limit = sqlite_limit(limit)?;
        let mut statement = self.connection.prepare(
            r#"WITH matches AS MATERIALIZED (
                   SELECT rowid AS version_id,
                          bm25(node_versions_fts) AS rank
                   FROM node_versions_fts
                   WHERE node_versions_fts MATCH ?1
                   ORDER BY rank, rowid
                   LIMIT ?8
               )
               SELECT n.id, v.row_id, v.revision, v.space_id, v.kind, v.label,
                      v.body, v.source, v.metadata, v.valid_from, v.valid_to,
                      v.recorded_at, v.superseded_at, v.agent_id, v.session_id,
                      n.last_accessed, n.access_count, n.salience, n.created_at,
                      n.source_weight, n.degree, n.salience_layer, n.token_len,
                      n.next_salience_at, matches.rank
               FROM matches
               JOIN node_versions AS v ON v.row_id = matches.version_id
               JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
               WHERE v.space_id = ?2
                 AND v.valid_from <= ?3
                 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
                 AND v.recorded_at <= ?4
                 AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
                 AND ((?5 AND n.salience_layer = 'hot')
                   OR (?6 AND n.salience_layer = 'warm')
                   OR (?7 AND n.salience_layer = 'cold'))
               ORDER BY matches.rank, v.row_id
               LIMIT ?9"#,
        )?;
        let rows = statement.query_map(
            params![
                query,
                space_id,
                as_of.valid_at,
                as_of.recorded_at,
                layers.hot,
                layers.warm,
                layers.cold,
                candidate_limit,
                limit
            ],
            |row| Ok((stored_node_from_row(row)?, row.get::<_, f64>(24)?)),
        )?;

        let mut hits = Vec::new();
        for row in rows {
            let (stored, bm25) = row?;
            hits.push(SearchHit {
                node: stored.into_node()?,
                bm25,
            });
        }
        Ok(hits)
    }

    pub(super) fn vector_search(
        &self,
        space_id: &str,
        model_id: &str,
        query_vector: &[f32],
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        if limit == 0 || !any_layer(layers) {
            return Ok(Vec::new());
        }
        if query_vector.iter().any(|value| !value.is_finite()) {
            return Err(StorageError::InvalidData(
                "query embedding values must all be finite".to_owned(),
            ));
        }
        let (table, dimensions) = match query_vector.len() {
            256 => ("node_embeddings_vec_256", 256),
            384 => ("node_embeddings_vec_384", 384),
            found => return Err(StorageError::UnsupportedEmbeddingDimensions { found }),
        };
        let overfetch = limit.saturating_mul(4).min(4_096);
        let sql = format!(
            r#"SELECT embedding_id, distance, embedding
               FROM {table}
               WHERE embedding MATCH ?1
                 AND k = ?2
                 AND space_id = ?3
                 AND model_id = ?4
               ORDER BY distance"#
        );
        let query_blob = vector_blob(query_vector);
        let mut statement = self.connection.prepare(&sql)?;
        let nearest = statement
            .query_map(
                params![query_blob, sqlite_limit(overfetch)?, space_id, model_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let mut hits = Vec::new();
        let mut node_statement = self.connection.prepare(
            r#"SELECT n.id, v.row_id, v.revision, v.space_id, v.kind, v.label,
                      v.body, v.source, v.metadata, v.valid_from, v.valid_to,
                      v.recorded_at, v.superseded_at, v.agent_id, v.session_id,
                      n.last_accessed, n.access_count, n.salience, n.created_at,
                      n.source_weight, n.degree, n.salience_layer, n.token_len,
                      n.next_salience_at
               FROM embedding_records AS records
               JOIN node_versions AS v ON v.row_id = records.node_version_id
               JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
               WHERE records.id = ?1
                 AND v.space_id = ?2
                 AND v.valid_from <= ?3
                 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
                 AND v.recorded_at <= ?4
                 AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
                 AND ((?5 AND n.salience_layer = 'hot')
                   OR (?6 AND n.salience_layer = 'warm')
                   OR (?7 AND n.salience_layer = 'cold'))"#,
        )?;
        for (record_id, distance, blob) in nearest {
            let stored = node_statement
                .query_row(
                    params![
                        record_id,
                        space_id,
                        as_of.valid_at,
                        as_of.recorded_at,
                        layers.hot,
                        layers.warm,
                        layers.cold
                    ],
                    stored_node_from_row,
                )
                .optional()?;
            if let Some(stored) = stored {
                hits.push(VectorSearchHit {
                    node: stored.into_node()?,
                    cosine: (1.0 - distance).clamp(-1.0, 1.0),
                    embedding: decode_vector_blob(&blob, dimensions)?,
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    pub(super) fn exact_match_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<Node>> {
        if limit == 0 || query.trim().is_empty() || !any_layer(layers) {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            r#"SELECT n.id, v.row_id, v.revision, v.space_id, v.kind, v.label,
                      v.body, v.source, v.metadata, v.valid_from, v.valid_to,
                      v.recorded_at, v.superseded_at, v.agent_id, v.session_id,
                      n.last_accessed, n.access_count, n.salience, n.created_at,
                      n.source_weight, n.degree, n.salience_layer, n.token_len,
                      n.next_salience_at
               FROM node_versions AS v
               JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
               WHERE v.space_id = ?1
                 AND v.valid_from <= ?3
                 AND (v.valid_to IS NULL OR ?3 < v.valid_to)
                 AND v.recorded_at <= ?4
                 AND (v.superseded_at IS NULL OR ?4 < v.superseded_at)
                 AND ((?5 AND n.salience_layer = 'hot')
                   OR (?6 AND n.salience_layer = 'warm')
                   OR (?7 AND n.salience_layer = 'cold'))
                 AND (
                     lower(v.label) = lower(trim(?2))
                     OR lower(v.node_id) = lower(trim(?2))
                     OR (v.kind IN ('entity', 'concept')
                         AND length(trim(v.label)) >= 2
                         AND instr(lower(?2), lower(trim(v.label))) > 0)
                     OR EXISTS (
                         SELECT 1
                         FROM json_each(v.metadata, '$.entities') AS entity
                         WHERE entity.type = 'text'
                           AND length(trim(entity.value)) >= 2
                           AND instr(lower(?2), lower(trim(entity.value))) > 0
                     )
                     OR EXISTS (
                         SELECT 1
                         FROM json_each(v.metadata, '$.tags') AS tag
                         WHERE tag.type = 'text'
                           AND length(trim(tag.value)) >= 2
                           AND instr(lower(?2), lower(trim(tag.value))) > 0
                     )
                 )
               ORDER BY lower(v.node_id) = lower(trim(?2)) DESC,
                        lower(v.label) = lower(trim(?2)) DESC,
                        length(v.label) DESC, v.row_id"#,
        )?;
        let rows = statement.query_map(
            params![
                space_id,
                query,
                as_of.valid_at,
                as_of.recorded_at,
                layers.hot,
                layers.warm,
                layers.cold
            ],
            stored_node_from_row,
        )?;
        let folded_query = folded_trimmed(query);
        let query_tokens = exact_tokens(query);
        let mut nodes = Vec::new();
        for row in rows {
            let node = row?.into_node()?;
            if exact_query_matches_node(&folded_query, &query_tokens, &node) {
                nodes.push(node);
                if nodes.len() >= limit {
                    break;
                }
            }
        }
        Ok(nodes)
    }

    pub(super) fn edge_as_of(
        &self,
        space_id: &str,
        edge_id: &str,
        as_of: AsOf,
    ) -> StorageResult<Option<Edge>> {
        self.connection
            .query_row(
                EDGE_AS_OF_SQL,
                params![space_id, edge_id, as_of.valid_at, as_of.recorded_at],
                stored_edge_from_row,
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub(super) fn materialized_paths_from(
        &self,
        space_id: &str,
        node_id: &str,
        limit: usize,
    ) -> StorageResult<Vec<MaterializedPath>> {
        let mut statement = self.connection.prepare(
            r#"SELECT space_id, a, b, c, ab_edge_id, bc_edge_id, path_weight,
                      path_kind, updated_at
               FROM materialized_paths
               WHERE space_id = ?1 AND a = ?2
               ORDER BY path_weight DESC, c, b
               LIMIT ?3"#,
        )?;
        let rows =
            statement.query_map(params![space_id, node_id, sqlite_limit(limit)?], |row| {
                Ok(MaterializedPath {
                    space_id: row.get(0)?,
                    from_node_id: row.get(1)?,
                    via_node_id: row.get(2)?,
                    to_node_id: row.get(3)?,
                    first_edge_id: row.get(4)?,
                    second_edge_id: row.get(5)?,
                    path_weight: row.get(6)?,
                    path_kind: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StorageError::from)
    }

    pub(super) fn materialized_path_count(&self, space_id: &str) -> StorageResult<usize> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM materialized_paths WHERE space_id = ?1",
            [space_id],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| StorageError::CorruptData {
            field: "materialized path count",
            value: count.to_string(),
        })
    }

    pub(super) fn maintenance(
        &mut self,
        now: Timestamp,
        batch_size: usize,
        allow_vacuum: bool,
    ) -> StorageResult<MaintenanceReport> {
        let batch_size = sqlite_limit(batch_size)?;
        let last_vacuum_at: Timestamp = self.connection.query_row(
            "SELECT last_vacuum_at FROM maintenance_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let salience_nodes = maintain_salience(&transaction, now, batch_size)?;
        let path_edges = maintain_paths(&transaction, now, batch_size)?;
        transaction.execute(
            "UPDATE maintenance_state SET last_tick_at = ?1 WHERE singleton = 1",
            [now],
        )?;
        transaction.commit()?;

        let vacuum_due = allow_vacuum
            && (last_vacuum_at == 0 || now.saturating_sub(last_vacuum_at) >= VACUUM_INTERVAL);
        if vacuum_due {
            self.connection.execute_batch("VACUUM")?;
            self.connection.execute(
                "UPDATE maintenance_state SET last_vacuum_at = ?1 WHERE singleton = 1",
                [now],
            )?;
        }
        Ok(MaintenanceReport {
            salience_nodes,
            path_edges,
            vacuumed: vacuum_due,
        })
    }

    pub(super) fn graph_neighbors(
        &self,
        space_id: &str,
        source_node_ids: &[String],
        as_of: AsOf,
        layers: SearchLayers,
        memory_only: bool,
        limit_per_source: usize,
    ) -> StorageResult<Vec<GraphNeighbor>> {
        let eligible_layers = [
            ("'hot'", layers.hot),
            ("'warm'", layers.warm),
            ("'cold'", layers.cold),
        ]
        .into_iter()
        .filter_map(|(name, enabled)| enabled.then_some(name))
        .collect::<Vec<_>>()
        .join(", ");
        if source_node_ids.is_empty() || limit_per_source == 0 || eligible_layers.is_empty() {
            return Ok(Vec::new());
        }
        let first_frontier_parameter = 2;
        let frontier_values = (0..source_node_ids.len())
            .map(|index| format!("(?{})", first_frontier_parameter + index))
            .collect::<Vec<_>>()
            .join(", ");
        let valid_at_parameter = first_frontier_parameter + source_node_ids.len();
        let recorded_at_parameter = valid_at_parameter + 1;
        let limit_parameter = recorded_at_parameter + 1;
        let memory_only_parameter = limit_parameter + 1;
        let sql = format!(
            r#"WITH wanted(node_id) AS (VALUES {frontier_values}),
               active_edges AS (
                   SELECT edge.from_node_id, edge.to_node_id, version.weight
                   FROM edges AS edge
                   JOIN edge_versions AS version
                     ON version.space_id = edge.space_id
                    AND version.edge_id = edge.id
                   WHERE edge.space_id = ?1
                     AND version.valid_from <= ?{valid_at_parameter}
                     AND (version.valid_to IS NULL OR ?{valid_at_parameter} < version.valid_to)
                     AND version.recorded_at <= ?{recorded_at_parameter}
                     AND (version.superseded_at IS NULL OR ?{recorded_at_parameter} < version.superseded_at)
                     AND version.weight > 0.0
               ),
               candidate_neighbors AS (
                   SELECT wanted.node_id AS source_node_id,
                          CASE WHEN edge.from_node_id = wanted.node_id
                               THEN edge.to_node_id ELSE edge.from_node_id END AS node_id,
                          edge.weight
                   FROM wanted
                   JOIN active_edges AS edge
                     ON edge.from_node_id = wanted.node_id
                     OR edge.to_node_id = wanted.node_id
               ),
               deduplicated AS (
                   SELECT source_node_id, node_id, max(weight) AS edge_weight
                   FROM candidate_neighbors AS candidate
                   WHERE node_id <> source_node_id
                     AND EXISTS (
                         SELECT 1
                         FROM nodes AS neighbor
                         JOIN node_versions AS neighbor_version
                           ON neighbor_version.space_id = neighbor.space_id
                          AND neighbor_version.node_id = neighbor.id
                         WHERE neighbor.space_id = ?1
                           AND neighbor.id = candidate.node_id
                           AND neighbor_version.valid_from <= ?{valid_at_parameter}
                           AND (neighbor_version.valid_to IS NULL OR ?{valid_at_parameter} < neighbor_version.valid_to)
                           AND neighbor_version.recorded_at <= ?{recorded_at_parameter}
                           AND (neighbor_version.superseded_at IS NULL OR ?{recorded_at_parameter} < neighbor_version.superseded_at)
                           AND neighbor.salience_layer IN ({eligible_layers})
                           AND (
                               ?{memory_only_parameter} = 0
                               OR CASE
                                   WHEN json_valid(neighbor_version.metadata)
                                   THEN coalesce(json_extract(neighbor_version.metadata, '$.brain_memory'), 0)
                                   ELSE 0
                               END = 1
                           )
                     )
                   GROUP BY source_node_id, node_id
               ),
               ranked AS (
                   SELECT source_node_id, node_id, edge_weight,
                          row_number() OVER (
                              PARTITION BY source_node_id
                              ORDER BY edge_weight DESC, node_id
                          ) AS position
                   FROM deduplicated
               )
               SELECT source_node_id, node_id, edge_weight
               FROM ranked
               WHERE position <= ?{limit_parameter}
               ORDER BY source_node_id, position"#,
        );
        let mut parameters = Vec::with_capacity(source_node_ids.len() + 5);
        parameters.push(Value::Text(space_id.to_owned()));
        parameters.extend(source_node_ids.iter().cloned().map(Value::Text));
        parameters.push(Value::Integer(as_of.valid_at));
        parameters.push(Value::Integer(as_of.recorded_at));
        parameters.push(Value::Integer(sqlite_limit(limit_per_source)?));
        parameters.push(Value::Integer(i64::from(memory_only)));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            Ok(GraphNeighbor {
                source_node_id: row.get(0)?,
                node_id: row.get(1)?,
                edge_weight: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StorageError::from)
    }

    pub(super) fn has_edges(&self, space_id: &str) -> StorageResult<bool> {
        let present: i64 = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM edges WHERE space_id = ?1)",
            [space_id],
            |row| row.get(0),
        )?;
        Ok(present != 0)
    }

    pub(super) fn rebuild_paths(&mut self, space_id: &str) -> StorageResult<usize> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        rebuild_materialized_paths(&transaction, space_id)?;
        let built: i64 = transaction.query_row(
            "SELECT count(*) FROM materialized_paths WHERE space_id = ?1",
            [space_id],
            |row| row.get(0),
        )?;
        transaction.commit()?;
        Ok(built.max(0) as usize)
    }

    pub(super) fn node_count(&self, space_id: &str) -> StorageResult<usize> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM nodes WHERE space_id = ?1",
            [space_id],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| StorageError::CorruptData {
            field: "node count",
            value: count.to_string(),
        })
    }

    pub(super) fn status(&self, space_id: &str, as_of: AsOf) -> StorageResult<StorageStatus> {
        let counts = self.connection.query_row(
            r#"WITH active_nodes AS (
                   SELECT n.salience_layer
                   FROM node_versions AS v
                   JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
                   WHERE v.space_id = ?1
                     AND v.valid_from <= ?2
                     AND (v.valid_to IS NULL OR ?2 < v.valid_to)
                     AND v.recorded_at <= ?3
                     AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
               ), active_edges AS (
                   SELECT e.id
                   FROM edge_versions AS v
                   JOIN edges AS e ON e.id = v.edge_id AND e.space_id = v.space_id
                   WHERE v.space_id = ?1
                     AND v.valid_from <= ?2
                     AND (v.valid_to IS NULL OR ?2 < v.valid_to)
                     AND v.recorded_at <= ?3
                     AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
               )
               SELECT
                   (SELECT count(*) FROM nodes WHERE space_id = ?1),
                   (SELECT count(*) FROM active_nodes),
                   (SELECT count(*) FROM edges WHERE space_id = ?1),
                   (SELECT count(*) FROM active_edges),
                   (SELECT count(*) FROM active_nodes WHERE salience_layer = 'hot'),
                   (SELECT count(*) FROM active_nodes WHERE salience_layer = 'warm'),
                   (SELECT count(*) FROM active_nodes WHERE salience_layer = 'cold'),
                   (SELECT count(*) FROM embedding_records WHERE space_id = ?1)"#,
            params![space_id, as_of.valid_at, as_of.recorded_at],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )?;
        let latest_node: Option<Timestamp> = self.connection.query_row(
            "SELECT max(recorded_at) FROM node_versions WHERE space_id = ?1",
            [space_id],
            |row| row.get(0),
        )?;
        let latest_edge: Option<Timestamp> = self.connection.query_row(
            "SELECT max(recorded_at) FROM edge_versions WHERE space_id = ?1",
            [space_id],
            |row| row.get(0),
        )?;
        Ok(StorageStatus {
            nodes: count_to_usize("node count", counts.0)?,
            active_nodes: count_to_usize("active node count", counts.1)?,
            edges: count_to_usize("edge count", counts.2)?,
            active_edges: count_to_usize("active edge count", counts.3)?,
            hot_nodes: count_to_usize("hot node count", counts.4)?,
            warm_nodes: count_to_usize("warm node count", counts.5)?,
            cold_nodes: count_to_usize("cold node count", counts.6)?,
            embeddings: count_to_usize("embedding count", counts.7)?,
            latest_recorded_at: latest_node.into_iter().chain(latest_edge).max(),
        })
    }

    pub(super) fn checkpoint(&self) -> StorageResult<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

fn actor_errors(count: usize, message: &str) -> Vec<StorageResult<()>> {
    (0..count)
        .map(|_| Err(StorageError::ActorUnavailable(message.to_owned())))
        .collect()
}

fn apply_write_operation(connection: &Connection, operation: WriteOperation) -> StorageResult<()> {
    match operation {
        WriteOperation::CreateAgent(agent) => create_agent(connection, &agent),
        WriteOperation::CreateSpace(space) => create_space(connection, &space),
        WriteOperation::CreateSession(session) => create_session(connection, &session),
        WriteOperation::EnsureContext {
            agent,
            space,
            session,
        } => ensure_context(connection, &agent, &space, &session),
        WriteOperation::InsertNodes(nodes) => insert_nodes(connection, &nodes),
        WriteOperation::ReplaceNode {
            space_id,
            node_id,
            replacement,
        } => replace_node(connection, &space_id, &node_id, &replacement),
        WriteOperation::InsertEdges { edges, bulk } => insert_edges(connection, &edges, bulk),
        WriteOperation::UpsertEmbeddings(embeddings) => {
            for embedding in &embeddings {
                upsert_embedding(connection, embedding)?;
            }
            Ok(())
        }
        WriteOperation::RecordNodeAccess {
            space_id,
            node_id,
            now,
        } => record_node_access(connection, &space_id, &node_id, now),
        WriteOperation::RecordNodeAccesses {
            space_id,
            node_ids,
            now,
        } => {
            for node_id in node_ids {
                record_node_access(connection, &space_id, &node_id, now)?;
            }
            Ok(())
        }
        WriteOperation::ForgetMemory {
            space_id,
            node_id,
            agent_id,
            session_id,
            valid_to,
            recorded_at,
        } => forget_memory(
            connection,
            &space_id,
            &node_id,
            &agent_id,
            &session_id,
            valid_to,
            recorded_at,
        ),
        WriteOperation::Noop => Ok(()),
    }
}

fn ensure_context(
    connection: &Connection,
    agent: &Agent,
    space: &Space,
    session: &Session,
) -> StorageResult<()> {
    if space.created_by_agent_id != agent.id {
        return Err(StorageError::InvalidData(
            "space creator must match the supplied agent".to_owned(),
        ));
    }
    if session.agent_id != agent.id || session.space_id != space.id {
        return Err(StorageError::InvalidData(
            "session attribution must match the supplied agent and space".to_owned(),
        ));
    }
    validate_id("agent id", &agent.id)?;
    validate_id("space id", &space.id)?;
    validate_id("session id", &session.id)?;
    validate_json_object(connection, &agent.metadata_json)?;
    validate_json_object(connection, &space.metadata_json)?;
    validate_json_object(connection, &session.metadata_json)?;
    connection.execute(
        r#"INSERT INTO agents(id, display_name, metadata, created_at)
           VALUES (?1, ?2, ?3, ?4)
           ON CONFLICT(id) DO NOTHING"#,
        params![
            agent.id,
            agent.display_name,
            agent.metadata_json,
            agent.created_at
        ],
    )?;
    connection.execute(
        r#"INSERT INTO spaces(
               id, kind, label, metadata, created_by_agent_id, created_at
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           ON CONFLICT(id) DO NOTHING"#,
        params![
            space.id,
            space.kind.as_str(),
            space.label,
            space.metadata_json,
            space.created_by_agent_id,
            space.created_at
        ],
    )?;
    connection.execute(
        r#"INSERT INTO sessions(
               id, space_id, agent_id, started_at, ended_at, metadata
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
           ON CONFLICT(id) DO NOTHING"#,
        params![
            session.id,
            session.space_id,
            session.agent_id,
            session.started_at,
            session.ended_at,
            session.metadata_json
        ],
    )?;
    Ok(())
}

fn create_agent(connection: &Connection, agent: &Agent) -> StorageResult<()> {
    validate_id("agent id", &agent.id)?;
    validate_json_object(connection, &agent.metadata_json)?;
    connection.execute(
        "INSERT INTO agents(id, display_name, metadata, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            agent.id,
            agent.display_name,
            agent.metadata_json,
            agent.created_at
        ],
    )?;
    Ok(())
}

fn create_space(connection: &Connection, space: &Space) -> StorageResult<()> {
    validate_id("space id", &space.id)?;
    validate_id("creating agent id", &space.created_by_agent_id)?;
    validate_json_object(connection, &space.metadata_json)?;
    connection.execute(
        r#"INSERT INTO spaces(
               id, kind, label, metadata, created_by_agent_id, created_at
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        params![
            space.id,
            space.kind.as_str(),
            space.label,
            space.metadata_json,
            space.created_by_agent_id,
            space.created_at
        ],
    )?;
    Ok(())
}

fn create_session(connection: &Connection, session: &Session) -> StorageResult<()> {
    validate_id("session id", &session.id)?;
    validate_id("session space id", &session.space_id)?;
    validate_id("session agent id", &session.agent_id)?;
    if session.ended_at.is_some_and(|end| end < session.started_at) {
        return Err(StorageError::InvalidData(
            "session ended_at precedes started_at".to_owned(),
        ));
    }
    validate_json_object(connection, &session.metadata_json)?;
    connection.execute(
        r#"INSERT INTO sessions(
               id, space_id, agent_id, started_at, ended_at, metadata
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        params![
            session.id,
            session.space_id,
            session.agent_id,
            session.started_at,
            session.ended_at,
            session.metadata_json
        ],
    )?;
    Ok(())
}

fn insert_nodes(connection: &Connection, nodes: &[NewNode]) -> StorageResult<()> {
    let mut metadata_values = HashSet::new();
    for node in nodes {
        validate_new_node_fields(node)?;
        metadata_values.insert(node.metadata_json.as_str());
    }
    for metadata in metadata_values {
        validate_json_object(connection, metadata)?;
    }

    let mut node_statement = connection.prepare(
        r#"INSERT INTO nodes(
               id, space_id, created_by_agent_id, created_in_session_id,
               created_at, last_accessed, access_count, salience,
               source_weight, degree, salience_layer, token_len,
               next_salience_at
           ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?5, 0, ?6, ?7, 0, ?8, ?9, ?10
           )"#,
    )?;
    let mut version_statement = connection.prepare(
        r#"INSERT INTO node_versions(
               node_id, space_id, revision, kind, label, body, source,
               metadata, valid_from, valid_to, recorded_at, superseded_at,
               agent_id, session_id
           ) VALUES (
               ?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, ?11, ?12
           )"#,
    )?;
    for node in nodes {
        let input = SalienceInput {
            kind: node.kind,
            source_weight: node.source_weight,
            created_at: node.attribution.recorded_at,
            last_accessed: node.attribution.recorded_at,
            access_count: 0,
            degree: 0,
        };
        let score = salience(input, node.attribution.recorded_at);
        let layer = layer_of(score);
        let next_salience_at = next_recalculation_at(input, node.attribution.recorded_at);
        let token_len = estimate_token_len(&node.label, &node.body);
        node_statement.execute(params![
            node.id,
            node.space_id,
            node.attribution.agent_id,
            node.attribution.session_id,
            node.attribution.recorded_at,
            score,
            node.source_weight,
            layer.as_str(),
            i64::try_from(token_len).unwrap_or(i64::MAX),
            next_salience_at
        ])?;
        version_statement.execute(params![
            node.id,
            node.space_id,
            node.kind.as_str(),
            node.label,
            node.body,
            node.source,
            node.metadata_json,
            node.valid_from,
            node.valid_to,
            node.attribution.recorded_at,
            node.attribution.agent_id,
            node.attribution.session_id
        ])?;
    }
    Ok(())
}

fn replace_node(
    connection: &Connection,
    space_id: &str,
    node_id: &str,
    replacement: &NodeReplacement,
) -> StorageResult<()> {
    validate_id("space id", space_id)?;
    validate_id("node id", node_id)?;
    validate_id("replacement agent id", &replacement.attribution.agent_id)?;
    validate_id(
        "replacement session id",
        &replacement.attribution.session_id,
    )?;
    validate_json_object(connection, &replacement.metadata_json)?;

    let current = current_node_version(connection, space_id, node_id, replacement.valid_from)?
        .ok_or_else(|| {
            StorageError::NotFound(format!(
                "node {node_id:?} has no current fact at valid time {}",
                replacement.valid_from
            ))
        })?;
    let latest_recorded_at: i64 = connection.query_row(
        "SELECT MAX(recorded_at) FROM node_versions WHERE node_id = ?1 AND space_id = ?2",
        params![node_id, space_id],
        |row| row.get(0),
    )?;
    if replacement.attribution.recorded_at <= latest_recorded_at {
        return Err(StorageError::InvalidData(format!(
            "replacement recorded_at must be later than {latest_recorded_at}"
        )));
    }
    let next_revision: i64 = connection.query_row(
        "SELECT MAX(revision) + 1 FROM node_versions WHERE node_id = ?1 AND space_id = ?2",
        params![node_id, space_id],
        |row| row.get(0),
    )?;
    connection.execute(
        "UPDATE node_versions SET superseded_at = ?1 WHERE row_id = ?2 AND superseded_at IS NULL",
        params![replacement.attribution.recorded_at, current.row_id],
    )?;

    let replacement_revision = if replacement.valid_from > current.valid_from {
        insert_node_version(
            connection,
            NewVersion {
                node_id,
                space_id,
                revision: next_revision,
                kind: &current.kind,
                label: &current.label,
                body: &current.body,
                source: current.source.as_deref(),
                metadata_json: &current.metadata_json,
                valid_from: current.valid_from,
                valid_to: Some(replacement.valid_from),
                recorded_at: replacement.attribution.recorded_at,
                agent_id: &replacement.attribution.agent_id,
                session_id: &replacement.attribution.session_id,
            },
        )?;
        next_revision + 1
    } else {
        next_revision
    };
    insert_node_version(
        connection,
        NewVersion {
            node_id,
            space_id,
            revision: replacement_revision,
            kind: replacement.kind.as_str(),
            label: &replacement.label,
            body: &replacement.body,
            source: replacement.source.as_deref(),
            metadata_json: &replacement.metadata_json,
            valid_from: replacement.valid_from,
            valid_to: current.valid_to,
            recorded_at: replacement.attribution.recorded_at,
            agent_id: &replacement.attribution.agent_id,
            session_id: &replacement.attribution.session_id,
        },
    )?;
    connection.execute(
        "UPDATE nodes SET token_len = ?1, next_salience_at = 0 WHERE space_id = ?2 AND id = ?3",
        params![
            i64::try_from(estimate_token_len(&replacement.label, &replacement.body))
                .unwrap_or(i64::MAX),
            space_id,
            node_id
        ],
    )?;
    Ok(())
}

fn forget_memory(
    connection: &Connection,
    space_id: &str,
    node_id: &str,
    agent_id: &str,
    session_id: &str,
    valid_to: Timestamp,
    recorded_at: Timestamp,
) -> StorageResult<()> {
    validate_id("space id", space_id)?;
    validate_id("node id", node_id)?;
    validate_id("agent id", agent_id)?;
    validate_id("session id", session_id)?;
    let current =
        current_node_version(connection, space_id, node_id, valid_to)?.ok_or_else(|| {
            StorageError::NotFound(format!("active memory {node_id:?} in space {space_id:?}"))
        })?;
    let (created_by, is_memory): (String, bool) = connection.query_row(
        r#"SELECT n.created_by_agent_id,
                  coalesce(json_extract(v.metadata, '$.brain_memory'), 0) = 1
           FROM nodes AS n
           JOIN node_versions AS v ON v.space_id = n.space_id AND v.node_id = n.id
           WHERE n.space_id = ?1 AND n.id = ?2 AND v.row_id = ?3"#,
        params![space_id, node_id, current.row_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if !is_memory {
        return Err(StorageError::InvalidData(format!(
            "node {node_id:?} is not a user memory"
        )));
    }
    if created_by != agent_id {
        return Err(StorageError::InvalidData(format!(
            "memory {node_id:?} belongs to agent {created_by:?}"
        )));
    }
    if valid_to <= current.valid_from {
        return Err(StorageError::InvalidData(
            "forget valid_to must be later than the memory valid_from".to_owned(),
        ));
    }
    let (latest_recorded_at, next_revision): (Timestamp, i64) = connection.query_row(
        r#"SELECT max(recorded_at), max(revision) + 1
           FROM node_versions WHERE node_id = ?1 AND space_id = ?2"#,
        params![node_id, space_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if recorded_at <= latest_recorded_at {
        return Err(StorageError::InvalidData(format!(
            "forget recorded_at must be later than {latest_recorded_at}"
        )));
    }
    connection.execute(
        "UPDATE node_versions SET superseded_at = ?1 WHERE row_id = ?2 AND superseded_at IS NULL",
        params![recorded_at, current.row_id],
    )?;
    insert_node_version(
        connection,
        NewVersion {
            node_id,
            space_id,
            revision: next_revision,
            kind: &current.kind,
            label: &current.label,
            body: &current.body,
            source: current.source.as_deref(),
            metadata_json: &current.metadata_json,
            valid_from: current.valid_from,
            valid_to: Some(valid_to),
            recorded_at,
            agent_id,
            session_id,
        },
    )?;
    connection.execute(
        r#"UPDATE nodes
           SET salience = 0.0, salience_layer = 'cold', next_salience_at = ?1
           WHERE space_id = ?2 AND id = ?3"#,
        params![recorded_at.saturating_add(7 * DAY), space_id, node_id],
    )?;
    Ok(())
}

fn insert_edges(connection: &Connection, edges: &[NewEdge], bulk: bool) -> StorageResult<()> {
    let mut degree_deltas: HashMap<(String, String), i64> = HashMap::new();
    let mut spaces = HashSet::new();
    let mut metadata_values = HashSet::new();
    for edge in edges {
        validate_new_edge_fields(edge)?;
        metadata_values.insert(edge.metadata_json.as_str());
    }
    for metadata in metadata_values {
        validate_json_object(connection, metadata)?;
    }
    let optimize_bulk = bulk && edges.len() >= 1_000;
    if optimize_bulk {
        drop_edge_indexes(connection)?;
        drop_materialized_path_indexes(connection)?;
    }

    let mut edge_statement = connection.prepare(
        r#"INSERT INTO edges(
               id, space_id, from_node_id, to_node_id, created_by_agent_id,
               created_in_session_id, created_at, last_accessed,
               access_count, salience
           ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, 0, ?8)"#,
    )?;
    let mut version_statement = connection.prepare(
        r#"INSERT INTO edge_versions(
               edge_id, space_id, revision, relation, weight, confidence,
               metadata, valid_from, valid_to, recorded_at, superseded_at,
               agent_id, session_id
           ) VALUES (
               ?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11
           )"#,
    )?;
    for edge in edges {
        edge_statement.execute(params![
            edge.id,
            edge.space_id,
            edge.from_node_id,
            edge.to_node_id,
            edge.attribution.agent_id,
            edge.attribution.session_id,
            edge.attribution.recorded_at,
            edge.salience
        ])?;
        version_statement.execute(params![
            edge.id,
            edge.space_id,
            edge.relation,
            edge.weight,
            edge.confidence,
            edge.metadata_json,
            edge.valid_from,
            edge.valid_to,
            edge.attribution.recorded_at,
            edge.attribution.agent_id,
            edge.attribution.session_id
        ])?;
        *degree_deltas
            .entry((edge.space_id.clone(), edge.from_node_id.clone()))
            .or_default() += 1;
        if edge.from_node_id != edge.to_node_id {
            *degree_deltas
                .entry((edge.space_id.clone(), edge.to_node_id.clone()))
                .or_default() += 1;
        }
        spaces.insert(edge.space_id.clone());
    }
    drop(edge_statement);
    drop(version_statement);

    let mut degree_statement = connection.prepare(
        "UPDATE nodes SET degree = degree + ?1, next_salience_at = 0 \
         WHERE space_id = ?2 AND id = ?3",
    )?;
    for ((space_id, node_id), delta) in degree_deltas {
        degree_statement.execute(params![delta, space_id, node_id])?;
    }
    drop(degree_statement);

    if bulk {
        if optimize_bulk {
            create_edge_indexes(connection)?;
        }
        for space_id in spaces {
            rebuild_materialized_paths(connection, &space_id)?;
        }
        if optimize_bulk {
            create_materialized_path_indexes(connection)?;
        }
    } else {
        for edge in edges {
            let endpoint_degree: i64 = connection.query_row(
                r#"SELECT coalesce(sum(degree), 0)
                   FROM nodes
                   WHERE space_id = ?1 AND id IN (?2, ?3)"#,
                params![edge.space_id, edge.from_node_id, edge.to_node_id],
                |row| row.get(0),
            )?;
            if endpoint_degree > MAX_INLINE_DEGREE {
                connection.execute(
                    r#"INSERT INTO path_update_queue(space_id, edge_id, queued_at)
                       VALUES (?1, ?2, ?3)
                       ON CONFLICT(space_id, edge_id) DO UPDATE SET queued_at = excluded.queued_at"#,
                    params![edge.space_id, edge.id, edge.attribution.recorded_at],
                )?;
            } else {
                refresh_materialized_paths_for_edge(
                    connection,
                    &edge.space_id,
                    &edge.id,
                    edge.attribution.recorded_at,
                )?;
            }
        }
    }
    Ok(())
}

fn upsert_embedding(connection: &Connection, embedding: &NewEmbedding) -> StorageResult<()> {
    validate_id("embedding space id", &embedding.space_id)?;
    validate_id("embedding model id", &embedding.model_id)?;
    validate_id("embedding agent id", &embedding.attribution.agent_id)?;
    validate_id("embedding session id", &embedding.attribution.session_id)?;
    if embedding.vector.iter().any(|value| !value.is_finite()) {
        return Err(StorageError::InvalidData(
            "embedding values must all be finite".to_owned(),
        ));
    }
    let dimensions = embedding.vector.len();
    if !matches!(dimensions, 256 | 384) {
        return Err(StorageError::UnsupportedEmbeddingDimensions { found: dimensions });
    }

    let existing = connection
        .query_row(
            r#"SELECT id, dimensions FROM embedding_records
               WHERE node_version_id = ?1 AND model_id = ?2"#,
            params![embedding.node_version_id, embedding.model_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let record_id = if let Some((record_id, old_dimensions)) = existing {
        let old_dimensions =
            usize::try_from(old_dimensions).map_err(|_| StorageError::CorruptData {
                field: "embedding dimensions",
                value: old_dimensions.to_string(),
            })?;
        delete_vector(connection, record_id, old_dimensions)?;
        connection.execute(
            r#"UPDATE embedding_records
               SET space_id = ?1, dimensions = ?2, embedded_at = ?3,
                   agent_id = ?4, session_id = ?5
               WHERE id = ?6"#,
            params![
                embedding.space_id,
                i64::try_from(dimensions).unwrap_or(i64::MAX),
                embedding.embedded_at,
                embedding.attribution.agent_id,
                embedding.attribution.session_id,
                record_id
            ],
        )?;
        record_id
    } else {
        connection.execute(
            r#"INSERT INTO embedding_records(
                   node_version_id, space_id, model_id, dimensions, embedded_at,
                   agent_id, session_id
               ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
            params![
                embedding.node_version_id,
                embedding.space_id,
                embedding.model_id,
                i64::try_from(dimensions).unwrap_or(i64::MAX),
                embedding.embedded_at,
                embedding.attribution.agent_id,
                embedding.attribution.session_id
            ],
        )?;
        connection.last_insert_rowid()
    };
    insert_vector(connection, record_id, embedding)
}

fn record_node_access(
    connection: &Connection,
    space_id: &str,
    node_id: &str,
    now: Timestamp,
) -> StorageResult<()> {
    let stored = connection
        .query_row(
            r#"SELECT v.kind, n.source_weight, n.created_at, n.access_count, n.degree
               FROM nodes AS n
               JOIN node_versions AS v
                 ON v.space_id = n.space_id AND v.node_id = n.id
               WHERE n.space_id = ?1 AND n.id = ?2
                 AND v.superseded_at IS NULL
               ORDER BY v.recorded_at DESC, v.revision DESC
               LIMIT 1"#,
            params![space_id, node_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StorageError::NotFound(format!("node {node_id:?} in {space_id:?}")))?;
    let kind = NodeKind::from_stored(&stored.0).ok_or_else(|| StorageError::CorruptData {
        field: "node kind",
        value: stored.0,
    })?;
    let input = SalienceInput {
        kind,
        source_weight: stored.1,
        created_at: stored.2,
        last_accessed: now,
        access_count: stored.3.saturating_add(1),
        degree: stored.4,
    };
    let score = salience(input, now);
    connection.execute(
        r#"UPDATE nodes
           SET last_accessed = ?1,
               access_count = access_count + 1,
               salience = ?2,
               salience_layer = ?3,
               next_salience_at = ?4
           WHERE space_id = ?5 AND id = ?6"#,
        params![
            now,
            score,
            layer_of(score).as_str(),
            next_recalculation_at(input, now),
            space_id,
            node_id
        ],
    )?;
    Ok(())
}

fn sqlite_limit(limit: usize) -> StorageResult<i64> {
    i64::try_from(limit)
        .map_err(|_| StorageError::InvalidData("limit does not fit in SQLite INTEGER".to_owned()))
}

fn count_to_usize(field: &'static str, count: i64) -> StorageResult<usize> {
    usize::try_from(count).map_err(|_| StorageError::CorruptData {
        field,
        value: count.to_string(),
    })
}

fn estimate_token_len(label: &str, body: &str) -> usize {
    label
        .len()
        .saturating_add(body.len())
        .saturating_add(1)
        .max(1)
}

const fn any_layer(layers: SearchLayers) -> bool {
    layers.hot || layers.warm || layers.cold
}

/// Applies the trust boundary for the `Exact` search signal.
///
/// SQLite's `instr` predicates above are deliberately only a cheap candidate
/// prefilter. Substrings are not exact evidence: the tag `ai`, for example,
/// must not match the word `brain`. We therefore validate labels, entities and
/// tags against whole normalized token sequences before search can award the
/// exact-match bonus or use the record as trusted template evidence.
fn exact_query_matches_node(folded_query: &str, query_tokens: &[String], node: &Node) -> bool {
    if folded_trimmed(&node.label) == folded_query || folded_trimmed(&node.id) == folded_query {
        return true;
    }
    if matches!(node.kind, NodeKind::Entity | NodeKind::Concept)
        && contains_exact_phrase(query_tokens, &node.label)
    {
        return true;
    }
    let Ok(metadata) = serde_json::from_str::<serde_json::Value>(&node.metadata_json) else {
        return false;
    };
    ["entities", "tags"].into_iter().any(|field| {
        metadata
            .get(field)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|values| {
                values.iter().any(|value| {
                    value
                        .as_str()
                        .is_some_and(|phrase| contains_exact_phrase(query_tokens, phrase))
                })
            })
    })
}

fn contains_exact_phrase(query_tokens: &[String], phrase: &str) -> bool {
    let phrase_tokens = exact_tokens(phrase);
    let phrase_characters = phrase_tokens
        .iter()
        .map(|token| token.chars().count())
        .sum::<usize>();
    phrase_characters >= 2
        && query_tokens
            .windows(phrase_tokens.len())
            .any(|window| window == phrase_tokens)
}

fn exact_tokens(text: &str) -> Vec<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .map(|token| token.chars().flat_map(char::to_lowercase).collect())
        .collect()
}

fn folded_trimmed(text: &str) -> String {
    text.trim().chars().flat_map(char::to_lowercase).collect()
}

fn vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector_blob(bytes: &[u8], dimensions: usize) -> StorageResult<Vec<f32>> {
    let expected_bytes = dimensions.saturating_mul(std::mem::size_of::<f32>());
    if bytes.len() != expected_bytes {
        return Err(StorageError::CorruptData {
            field: "embedding blob length",
            value: bytes.len().to_string(),
        });
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn delete_vector(connection: &Connection, record_id: i64, dimensions: usize) -> StorageResult<()> {
    match dimensions {
        256 => {
            connection.execute(
                "DELETE FROM node_embeddings_vec_256 WHERE embedding_id = ?1",
                [record_id],
            )?;
        }
        384 => {
            connection.execute(
                "DELETE FROM node_embeddings_vec_384 WHERE embedding_id = ?1",
                [record_id],
            )?;
        }
        found => return Err(StorageError::UnsupportedEmbeddingDimensions { found }),
    }
    Ok(())
}

fn insert_vector(
    connection: &Connection,
    record_id: i64,
    embedding: &NewEmbedding,
) -> StorageResult<()> {
    let blob = vector_blob(&embedding.vector);
    let sql = match embedding.vector.len() {
        256 => {
            "INSERT INTO node_embeddings_vec_256(embedding_id, space_id, model_id, embedding) \
             VALUES (?1, ?2, ?3, ?4)"
        }
        384 => {
            "INSERT INTO node_embeddings_vec_384(embedding_id, space_id, model_id, embedding) \
             VALUES (?1, ?2, ?3, ?4)"
        }
        found => return Err(StorageError::UnsupportedEmbeddingDimensions { found }),
    };
    connection.execute(
        sql,
        params![record_id, embedding.space_id, embedding.model_id, blob],
    )?;
    Ok(())
}

fn refresh_materialized_paths_for_edge(
    connection: &Connection,
    space_id: &str,
    edge_id: &str,
    now: Timestamp,
) -> StorageResult<()> {
    let edge = connection
        .query_row(
            r#"SELECT e.from_node_id, e.to_node_id, v.weight, v.relation
               FROM edges AS e
               JOIN edge_versions AS v
                 ON v.space_id = e.space_id AND v.edge_id = e.id
               WHERE e.space_id = ?1 AND e.id = ?2
                 AND v.superseded_at IS NULL
               ORDER BY v.recorded_at DESC, v.revision DESC
               LIMIT 1"#,
            params![space_id, edge_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((from_node_id, to_node_id, weight, relation)) = edge else {
        return Ok(());
    };

    connection.execute(
        r#"DELETE FROM materialized_paths
           WHERE space_id = ?1 AND (ab_edge_id = ?2 OR bc_edge_id = ?2)"#,
        params![space_id, edge_id],
    )?;

    connection.execute(
        r#"INSERT INTO materialized_paths(
               space_id, a, b, c, ab_edge_id, bc_edge_id, path_weight,
               path_kind, created_at, updated_at
           )
           SELECT ?1, ?2, ?3, outgoing.to_node_id, ?4, outgoing.id,
                  ?5 * outgoing_version.weight,
                  ?6 || '::' || outgoing_version.relation, ?7, ?7
           FROM edges AS outgoing
           JOIN edge_versions AS outgoing_version
             ON outgoing_version.space_id = outgoing.space_id
            AND outgoing_version.edge_id = outgoing.id
           WHERE outgoing.space_id = ?1
             AND outgoing.from_node_id = ?3
             AND outgoing_version.superseded_at IS NULL
             AND true
           ON CONFLICT(space_id, a, b, c) DO UPDATE SET
               ab_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.ab_edge_id ELSE materialized_paths.ab_edge_id END,
               bc_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.bc_edge_id ELSE materialized_paths.bc_edge_id END,
               path_kind = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.path_kind ELSE materialized_paths.path_kind END,
               path_weight = max(materialized_paths.path_weight, excluded.path_weight),
               updated_at = excluded.updated_at"#,
        params![
            space_id,
            from_node_id,
            to_node_id,
            edge_id,
            weight,
            relation,
            now
        ],
    )?;

    connection.execute(
        r#"INSERT INTO materialized_paths(
               space_id, a, b, c, ab_edge_id, bc_edge_id, path_weight,
               path_kind, created_at, updated_at
           )
           SELECT ?1, incoming.from_node_id, ?2, ?3, incoming.id, ?4,
                  incoming_version.weight * ?5,
                  incoming_version.relation || '::' || ?6, ?7, ?7
           FROM edges AS incoming
           JOIN edge_versions AS incoming_version
             ON incoming_version.space_id = incoming.space_id
            AND incoming_version.edge_id = incoming.id
           WHERE incoming.space_id = ?1
             AND incoming.to_node_id = ?2
             AND incoming_version.superseded_at IS NULL
             AND true
           ON CONFLICT(space_id, a, b, c) DO UPDATE SET
               ab_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.ab_edge_id ELSE materialized_paths.ab_edge_id END,
               bc_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.bc_edge_id ELSE materialized_paths.bc_edge_id END,
               path_kind = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.path_kind ELSE materialized_paths.path_kind END,
               path_weight = max(materialized_paths.path_weight, excluded.path_weight),
               updated_at = excluded.updated_at"#,
        params![
            space_id,
            from_node_id,
            to_node_id,
            edge_id,
            weight,
            relation,
            now
        ],
    )?;
    Ok(())
}

fn rebuild_materialized_paths(connection: &Connection, space_id: &str) -> StorageResult<()> {
    connection.execute_batch(
        r#"DROP TABLE IF EXISTS temp.current_edges_for_paths;
           CREATE TEMP TABLE current_edges_for_paths (
               edge_id      TEXT PRIMARY KEY,
               from_node_id TEXT NOT NULL,
               to_node_id   TEXT NOT NULL,
               weight       REAL NOT NULL,
               relation     TEXT NOT NULL,
               recorded_at  INTEGER NOT NULL
           ) STRICT;"#,
    )?;
    connection.execute(
        r#"INSERT INTO temp.current_edges_for_paths(
               edge_id, from_node_id, to_node_id, weight, relation, recorded_at
           )
           SELECT edge.id, edge.from_node_id, edge.to_node_id,
                  version.weight, version.relation, version.recorded_at
           FROM edges AS edge
           JOIN edge_versions AS version
             ON version.space_id = edge.space_id
            AND version.edge_id = edge.id
            AND version.superseded_at IS NULL
           WHERE edge.space_id = ?1"#,
        [space_id],
    )?;
    connection.execute_batch(
        r#"CREATE INDEX temp.idx_current_edges_from
               ON current_edges_for_paths(from_node_id, to_node_id);"#,
    )?;
    connection.execute(
        "DELETE FROM materialized_paths WHERE space_id = ?1",
        [space_id],
    )?;
    connection.execute(
        r#"INSERT INTO materialized_paths(
               space_id, a, b, c, ab_edge_id, bc_edge_id, path_weight,
               path_kind, created_at, updated_at
           )
           SELECT ?1,
                  first.from_node_id,
                  first.to_node_id,
                  second.to_node_id,
                  first.edge_id,
                  second.edge_id,
                  first.weight * second.weight,
                  first.relation || '::' || second.relation,
                  max(first.recorded_at, second.recorded_at),
                  max(first.recorded_at, second.recorded_at)
           FROM temp.current_edges_for_paths AS first
           JOIN temp.current_edges_for_paths AS second
             ON second.from_node_id = first.to_node_id
           WHERE true
           ON CONFLICT(space_id, a, b, c) DO UPDATE SET
               ab_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.ab_edge_id ELSE materialized_paths.ab_edge_id END,
               bc_edge_id = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.bc_edge_id ELSE materialized_paths.bc_edge_id END,
               path_kind = CASE
                   WHEN excluded.path_weight > materialized_paths.path_weight
                   THEN excluded.path_kind ELSE materialized_paths.path_kind END,
               path_weight = max(materialized_paths.path_weight, excluded.path_weight),
               updated_at = excluded.updated_at"#,
        [space_id],
    )?;
    connection.execute_batch("DROP TABLE temp.current_edges_for_paths;")?;
    Ok(())
}

fn drop_materialized_path_indexes(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(
        r#"DROP INDEX IF EXISTS idx_paths_from;
           DROP INDEX IF EXISTS idx_paths_to;
           DROP INDEX IF EXISTS idx_paths_hub;
           DROP INDEX IF EXISTS idx_paths_kind;"#,
    )?;
    Ok(())
}

fn create_materialized_path_indexes(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(
        r#"CREATE INDEX idx_paths_from
               ON materialized_paths(space_id, a, path_weight DESC, c);
           CREATE INDEX idx_paths_to
               ON materialized_paths(space_id, c, path_weight DESC, a);
           CREATE INDEX idx_paths_hub
               ON materialized_paths(space_id, b);
           CREATE INDEX idx_paths_kind
               ON materialized_paths(space_id, path_kind, a, c);"#,
    )?;
    Ok(())
}

fn drop_edge_indexes(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(
        r#"DROP INDEX IF EXISTS idx_edges_space_endpoints;
           DROP INDEX IF EXISTS idx_edge_versions_space_relation;
           DROP INDEX IF EXISTS idx_edge_versions_agent_time;
           DROP INDEX IF EXISTS idx_edge_versions_as_of;
           DROP INDEX IF EXISTS idx_edge_versions_valid_time;
           DROP INDEX IF EXISTS idx_edge_versions_recorded_time;
           DROP INDEX IF EXISTS idx_edges_decay;"#,
    )?;
    Ok(())
}

fn create_edge_indexes(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(
        r#"CREATE INDEX idx_edges_space_endpoints
               ON edges(space_id, from_node_id, to_node_id);
           CREATE INDEX idx_edge_versions_space_relation
               ON edge_versions(space_id, relation, edge_id);
           CREATE INDEX idx_edge_versions_agent_time
               ON edge_versions(agent_id, recorded_at);
           CREATE INDEX idx_edge_versions_as_of
               ON edge_versions(edge_id, space_id, valid_from, valid_to,
                                recorded_at, superseded_at);
           CREATE INDEX idx_edge_versions_valid_time
               ON edge_versions(space_id, valid_from, valid_to);
           CREATE INDEX idx_edge_versions_recorded_time
               ON edge_versions(space_id, recorded_at, superseded_at);
           CREATE INDEX idx_edges_decay
               ON edges(space_id, salience, last_accessed, access_count);"#,
    )?;
    Ok(())
}

fn maintain_salience(
    connection: &Connection,
    now: Timestamp,
    batch_size: i64,
) -> StorageResult<usize> {
    let mut statement = connection.prepare(
        r#"SELECT n.space_id, n.id, v.kind, n.source_weight, n.created_at,
                  n.last_accessed, n.access_count, n.degree
           FROM nodes AS n
           JOIN node_versions AS v
             ON v.space_id = n.space_id AND v.node_id = n.id
           WHERE n.next_salience_at <= ?1
             AND v.superseded_at IS NULL
             AND v.row_id = (
                 SELECT latest.row_id
                 FROM node_versions AS latest
                 WHERE latest.space_id = n.space_id
                   AND latest.node_id = n.id
                   AND latest.superseded_at IS NULL
                 ORDER BY latest.recorded_at DESC, latest.revision DESC
                 LIMIT 1
             )
           ORDER BY n.next_salience_at, n.space_id, n.id
           LIMIT ?2"#,
    )?;
    let due = statement
        .query_map(params![now, batch_size], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    for (space_id, node_id, kind, source_weight, created_at, last_accessed, access_count, degree) in
        &due
    {
        let kind = NodeKind::from_stored(kind).ok_or_else(|| StorageError::CorruptData {
            field: "node kind",
            value: kind.clone(),
        })?;
        let input = SalienceInput {
            kind,
            source_weight: *source_weight,
            created_at: *created_at,
            last_accessed: *last_accessed,
            access_count: *access_count,
            degree: *degree,
        };
        let score = salience(input, now);
        connection.execute(
            r#"UPDATE nodes
               SET salience = ?1, salience_layer = ?2, next_salience_at = ?3
               WHERE space_id = ?4 AND id = ?5"#,
            params![
                score,
                layer_of(score).as_str(),
                next_recalculation_at(input, now),
                space_id,
                node_id
            ],
        )?;
    }
    Ok(due.len())
}

fn maintain_paths(
    connection: &Connection,
    now: Timestamp,
    batch_size: i64,
) -> StorageResult<usize> {
    let mut statement = connection.prepare(
        r#"SELECT space_id, edge_id
           FROM path_update_queue
           WHERE queued_at <= ?1
           ORDER BY queued_at, space_id, edge_id
           LIMIT ?2"#,
    )?;
    let queued = statement
        .query_map(params![now, batch_size], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for (space_id, edge_id) in &queued {
        refresh_materialized_paths_for_edge(connection, space_id, edge_id, now)?;
        connection.execute(
            "DELETE FROM path_update_queue WHERE space_id = ?1 AND edge_id = ?2",
            params![space_id, edge_id],
        )?;
    }
    Ok(queued.len())
}

/// Read-only schema check: the version and the whole migration ledger must be
/// exactly this build's, otherwise the caller must not read (nothing is migrated).
fn verify_current_schema(connection: &Connection) -> StorageResult<()> {
    let version = read_schema_version(connection)?;
    if version != SCHEMA_VERSION {
        return Err(StorageError::InvalidData(format!(
            "database schema version {version} is not {SCHEMA_VERSION}; a read-only open never migrates"
        )));
    }
    let applied = connection
        .prepare("SELECT version, name FROM schema_migrations ORDER BY version")?
        .query_map([], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let expected = MIGRATIONS
        .iter()
        .map(|migration| (migration.version, migration.name.to_owned()))
        .collect::<Vec<_>>();
    if applied != expected {
        return Err(StorageError::InvalidData(
            "schema_migrations does not match this build; a read-only open never migrates"
                .to_owned(),
        ));
    }
    Ok(())
}

fn configure_connection(connection: &Connection) -> StorageResult<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;\n\
         PRAGMA journal_mode = WAL;\n\
         PRAGMA synchronous = NORMAL;",
    )?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> StorageResult<()> {
    let user_version = read_schema_version(connection)?;
    if user_version > SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            found: user_version,
            supported: SCHEMA_VERSION,
        });
    }

    let ledger_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    if !ledger_exists && user_version != 0 {
        return Err(StorageError::InvalidData(format!(
            "schema version is {user_version}, but schema_migrations is missing"
        )));
    }
    if !ledger_exists {
        connection.execute_batch(
            r#"CREATE TABLE schema_migrations (
                   version     INTEGER PRIMARY KEY CHECK (version > 0),
                   name        TEXT NOT NULL UNIQUE,
                   applied_at  INTEGER NOT NULL
               ) STRICT;"#,
        )?;
    }

    let applied = {
        let mut statement =
            connection.prepare("SELECT version, name FROM schema_migrations ORDER BY version")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    if applied.len() != usize::try_from(user_version).unwrap_or(usize::MAX) {
        return Err(StorageError::InvalidData(format!(
            "schema_migrations has {} rows but user_version is {user_version}",
            applied.len()
        )));
    }
    for (index, (version, name)) in applied.iter().enumerate() {
        let expected = MIGRATIONS.get(index).ok_or(StorageError::SchemaTooNew {
            found: *version,
            supported: SCHEMA_VERSION,
        })?;
        if *version != expected.version || name != expected.name {
            return Err(StorageError::InvalidData(format!(
                "migration {version} is named {name:?}, expected {:?}",
                expected.name
            )));
        }
    }

    for migration in MIGRATIONS.iter().skip(applied.len()) {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            r#"INSERT INTO schema_migrations(version, name, applied_at)
               VALUES (?1, ?2,
                       CAST((julianday('now') - 2440587.5) * 86400 AS INTEGER))"#,
            params![migration.version, migration.name],
        )?;
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
    }

    Ok(())
}

fn read_schema_version(connection: &Connection) -> StorageResult<u32> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    u32::try_from(version).map_err(|_| {
        StorageError::InvalidData(format!("PRAGMA user_version is outside u32: {version}"))
    })
}

fn validate_id(field: &str, value: &str) -> StorageResult<()> {
    if value.trim().is_empty() {
        return Err(StorageError::InvalidData(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(())
}

fn validate_json_object(connection: &Connection, value: &str) -> StorageResult<()> {
    let valid: bool = connection.query_row(
        "SELECT json_valid(?1) AND json_type(?1) = 'object'",
        [value],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(StorageError::InvalidData(
            "metadata_json must contain one JSON object".to_owned(),
        ));
    }
    Ok(())
}

fn validate_interval(from: i64, to: Option<i64>) -> StorageResult<()> {
    if to.is_some_and(|end| end <= from) {
        return Err(StorageError::InvalidData(
            "valid_to must be later than valid_from".to_owned(),
        ));
    }
    Ok(())
}

fn validate_unit_interval(field: &str, value: f64) -> StorageResult<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(StorageError::InvalidData(format!(
            "{field} must be a finite number from 0 through 1"
        )));
    }
    Ok(())
}

fn validate_new_node_fields(node: &NewNode) -> StorageResult<()> {
    validate_id("node id", &node.id)?;
    validate_id("node space id", &node.space_id)?;
    validate_id("node agent id", &node.attribution.agent_id)?;
    validate_id("node session id", &node.attribution.session_id)?;
    validate_interval(node.valid_from, node.valid_to)?;
    validate_unit_interval("salience", node.salience)?;
    validate_unit_interval("source_weight", node.source_weight)
}

fn validate_new_edge_fields(edge: &NewEdge) -> StorageResult<()> {
    validate_id("edge id", &edge.id)?;
    validate_id("edge space id", &edge.space_id)?;
    validate_id("edge source node id", &edge.from_node_id)?;
    validate_id("edge target node id", &edge.to_node_id)?;
    validate_id("edge relation", &edge.relation)?;
    validate_id("edge agent id", &edge.attribution.agent_id)?;
    validate_id("edge session id", &edge.attribution.session_id)?;
    validate_interval(edge.valid_from, edge.valid_to)?;
    validate_unit_interval("weight", edge.weight)?;
    validate_unit_interval("confidence", edge.confidence)?;
    validate_unit_interval("salience", edge.salience)
}

struct CurrentNodeVersion {
    row_id: i64,
    kind: String,
    label: String,
    body: String,
    source: Option<String>,
    metadata_json: String,
    valid_from: i64,
    valid_to: Option<i64>,
}

fn current_node_version(
    connection: &Connection,
    space_id: &str,
    node_id: &str,
    valid_at: i64,
) -> StorageResult<Option<CurrentNodeVersion>> {
    connection
        .query_row(
            r#"SELECT row_id, kind, label, body, source, metadata,
                      valid_from, valid_to
               FROM node_versions
               WHERE node_id = ?1 AND space_id = ?2
                 AND superseded_at IS NULL
                 AND valid_from <= ?3
                 AND (valid_to IS NULL OR ?3 < valid_to)
               ORDER BY recorded_at DESC, revision DESC
               LIMIT 1"#,
            params![node_id, space_id, valid_at],
            |row| {
                Ok(CurrentNodeVersion {
                    row_id: row.get(0)?,
                    kind: row.get(1)?,
                    label: row.get(2)?,
                    body: row.get(3)?,
                    source: row.get(4)?,
                    metadata_json: row.get(5)?,
                    valid_from: row.get(6)?,
                    valid_to: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(StorageError::from)
}

struct NewVersion<'a> {
    node_id: &'a str,
    space_id: &'a str,
    revision: i64,
    kind: &'a str,
    label: &'a str,
    body: &'a str,
    source: Option<&'a str>,
    metadata_json: &'a str,
    valid_from: i64,
    valid_to: Option<i64>,
    recorded_at: i64,
    agent_id: &'a str,
    session_id: &'a str,
}

fn insert_node_version(connection: &Connection, version: NewVersion<'_>) -> StorageResult<()> {
    connection.execute(
        r#"INSERT INTO node_versions(
               node_id, space_id, revision, kind, label, body, source, metadata,
               valid_from, valid_to, recorded_at, superseded_at, agent_id, session_id
           ) VALUES (
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12, ?13
           )"#,
        params![
            version.node_id,
            version.space_id,
            version.revision,
            version.kind,
            version.label,
            version.body,
            version.source,
            version.metadata_json,
            version.valid_from,
            version.valid_to,
            version.recorded_at,
            version.agent_id,
            version.session_id
        ],
    )?;
    Ok(())
}

struct StoredNode {
    id: String,
    version_id: i64,
    revision: i64,
    space_id: String,
    kind: String,
    label: String,
    body: String,
    source: Option<String>,
    metadata_json: String,
    valid_from: i64,
    valid_to: Option<i64>,
    recorded_at: i64,
    superseded_at: Option<i64>,
    agent_id: String,
    session_id: String,
    last_accessed: i64,
    access_count: i64,
    salience: f64,
    created_at: i64,
    source_weight: f64,
    degree: i64,
    salience_layer: String,
    token_len: i64,
    next_salience_at: i64,
}

impl StoredNode {
    fn into_node(self) -> StorageResult<Node> {
        let kind = NodeKind::from_stored(&self.kind).ok_or_else(|| StorageError::CorruptData {
            field: "node kind",
            value: self.kind.clone(),
        })?;
        let salience_layer = SalienceLayer::from_stored(&self.salience_layer).ok_or_else(|| {
            StorageError::CorruptData {
                field: "salience layer",
                value: self.salience_layer.clone(),
            }
        })?;
        let token_len = usize::try_from(self.token_len).map_err(|_| StorageError::CorruptData {
            field: "token length",
            value: self.token_len.to_string(),
        })?;
        Ok(Node {
            id: self.id,
            version_id: self.version_id,
            revision: self.revision,
            space_id: self.space_id,
            kind,
            label: self.label,
            body: self.body,
            source: self.source,
            metadata_json: self.metadata_json,
            valid_from: self.valid_from,
            valid_to: self.valid_to,
            recorded_at: self.recorded_at,
            superseded_at: self.superseded_at,
            agent_id: self.agent_id,
            session_id: self.session_id,
            last_accessed: self.last_accessed,
            access_count: self.access_count,
            salience: self.salience,
            created_at: self.created_at,
            source_weight: self.source_weight,
            degree: self.degree,
            salience_layer,
            token_len,
            next_salience_at: self.next_salience_at,
        })
    }
}

fn stored_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredNode> {
    Ok(StoredNode {
        id: row.get(0)?,
        version_id: row.get(1)?,
        revision: row.get(2)?,
        space_id: row.get(3)?,
        kind: row.get(4)?,
        label: row.get(5)?,
        body: row.get(6)?,
        source: row.get(7)?,
        metadata_json: row.get(8)?,
        valid_from: row.get(9)?,
        valid_to: row.get(10)?,
        recorded_at: row.get(11)?,
        superseded_at: row.get(12)?,
        agent_id: row.get(13)?,
        session_id: row.get(14)?,
        last_accessed: row.get(15)?,
        access_count: row.get(16)?,
        salience: row.get(17)?,
        created_at: row.get(18)?,
        source_weight: row.get(19)?,
        degree: row.get(20)?,
        salience_layer: row.get(21)?,
        token_len: row.get(22)?,
        next_salience_at: row.get(23)?,
    })
}

fn stored_edge_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    Ok(Edge {
        id: row.get(0)?,
        version_id: row.get(1)?,
        revision: row.get(2)?,
        space_id: row.get(3)?,
        from_node_id: row.get(4)?,
        to_node_id: row.get(5)?,
        relation: row.get(6)?,
        weight: row.get(7)?,
        confidence: row.get(8)?,
        metadata_json: row.get(9)?,
        valid_from: row.get(10)?,
        valid_to: row.get(11)?,
        recorded_at: row.get(12)?,
        superseded_at: row.get(13)?,
        agent_id: row.get(14)?,
        session_id: row.get(15)?,
        last_accessed: row.get(16)?,
        access_count: row.get(17)?,
        salience: row.get(18)?,
    })
}

fn register_sqlite_vec() -> StorageResult<()> {
    type ExtensionEntryPoint = unsafe extern "C" fn(
        *mut rusqlite::ffi::sqlite3,
        *mut *mut c_char,
        *const rusqlite::ffi::sqlite3_api_routines,
    ) -> c_int;

    static REGISTRATION_RESULT: OnceLock<c_int> = OnceLock::new();
    let result = *REGISTRATION_RESULT.get_or_init(|| {
        // sqlite-vec exposes the standard SQLite extension entry point with a
        // deliberately opaque declaration. SQLite's API requires the concrete
        // three-argument function type used here.
        unsafe {
            let entry_point = std::mem::transmute::<*const (), ExtensionEntryPoint>(
                sqlite_vec::sqlite3_vec_init as *const (),
            );
            rusqlite::ffi::sqlite3_auto_extension(Some(entry_point))
        }
    });
    if result != rusqlite::ffi::SQLITE_OK {
        return Err(StorageError::VectorExtension(result));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribution, NodeKind, SpaceKind, SqliteStorage, Storage};

    const AGENT_ID: &str = "test-agent";
    const SPACE_ID: &str = "test-space";
    const SESSION_ID: &str = "test-session";

    fn test_storage() -> SqliteStorage {
        let storage = SqliteStorage::open_in_memory().expect("open test database");
        storage
            .create_agent(&Agent {
                id: AGENT_ID.to_owned(),
                display_name: Some("Test Agent".to_owned()),
                metadata_json: "{}".to_owned(),
                created_at: 10,
            })
            .expect("create agent");
        storage
            .create_space(&Space {
                id: SPACE_ID.to_owned(),
                kind: SpaceKind::Project,
                label: "Test Space".to_owned(),
                metadata_json: "{}".to_owned(),
                created_by_agent_id: AGENT_ID.to_owned(),
                created_at: 20,
            })
            .expect("create space");
        storage
            .create_session(&Session {
                id: SESSION_ID.to_owned(),
                space_id: SPACE_ID.to_owned(),
                agent_id: AGENT_ID.to_owned(),
                started_at: 30,
                ended_at: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("create session");
        storage
    }

    fn attribution(recorded_at: i64) -> Attribution {
        Attribution {
            agent_id: AGENT_ID.to_owned(),
            session_id: SESSION_ID.to_owned(),
            recorded_at,
        }
    }

    fn node(id: &str, label: &str, body: &str, valid_from: i64, recorded_at: i64) -> NewNode {
        NewNode {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            kind: NodeKind::Memory,
            label: label.to_owned(),
            body: body.to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from,
            valid_to: None,
            attribution: attribution(recorded_at),
            salience: 0.8,
            source_weight: 0.8,
        }
    }

    #[test]
    fn exact_metadata_and_entity_matches_respect_token_boundaries() {
        let storage = test_storage();
        let mut tagged = node(
            "tagged-record",
            "Unrelated potato recipe",
            "Bake until crisp",
            100,
            110,
        );
        tagged.metadata_json =
            r#"{"tags":["ai","machine learning","ёж"],"entities":[]}"#.to_owned();
        let mut concept = node("short-concept", "AI", "A short concept", 100, 110);
        concept.kind = NodeKind::Concept;
        let identifier = node(
            "mem_exact_identifier",
            "An opaque record",
            "The identifier is not repeated in the body",
            100,
            110,
        );
        for record in [&tagged, &concept, &identifier] {
            storage.insert_node(record).expect("insert exact fixture");
        }

        for false_positive in ["brain submarine arctic", "платёж готов"] {
            let hits = storage
                .exact_match_nodes(
                    SPACE_ID,
                    false_positive,
                    AsOf::new(200, 200),
                    SearchLayers::ALL,
                    10,
                )
                .expect("search substring false positive");
            assert!(
                hits.is_empty(),
                "substring {false_positive:?} must not manufacture Exact hits: {hits:?}"
            );
        }

        for (query, expected_id) in [
            ("AI architecture", "tagged-record"),
            ("Notes on MACHINE learning", "tagged-record"),
            ("ёж проект", "tagged-record"),
            ("mem_exact_identifier", "mem_exact_identifier"),
        ] {
            let hits = storage
                .exact_match_nodes(SPACE_ID, query, AsOf::new(200, 200), SearchLayers::ALL, 10)
                .expect("search exact fixture");
            assert!(
                hits.iter().any(|hit| hit.id == expected_id),
                "whole token or identifier {query:?} did not match {expected_id}: {hits:?}"
            );
        }
    }

    #[test]
    fn migrations_create_versioned_fts_and_vector_schema() {
        let mut storage = SqliteDatabase::open_in_memory().expect("open direct test database");
        assert_eq!(
            storage.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );

        migrate(&mut storage.connection).expect("migrations are idempotent");
        let migration_count: i64 = storage
            .connection
            .query_row("SELECT count(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("count migrations");
        assert_eq!(migration_count, i64::from(SCHEMA_VERSION));

        for object in [
            "nodes",
            "node_versions",
            "edges",
            "edge_versions",
            "node_versions_fts",
            "node_embeddings_vec",
            "node_embeddings_vec_256",
            "node_embeddings_vec_384",
            "materialized_paths",
            "idx_node_versions_as_of",
            "idx_node_versions_agent_time",
            "idx_nodes_decay",
            "idx_nodes_space_layer_next",
        ] {
            let exists: bool = storage
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                    [object],
                    |row| row.get(0),
                )
                .expect("inspect schema object");
            assert!(exists, "missing schema object: {object}");
        }

        let vec_version: String = storage
            .connection
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("sqlite-vec is registered");
        assert!(vec_version.starts_with('v'));

        let vector = format!("[{}]", vec!["0.0"; 256].join(","));
        storage
            .connection
            .execute(
                "INSERT INTO node_embeddings_vec(embedding_id, embedding) VALUES (1, ?1)",
                [vector],
            )
            .expect("insert vector");
        let dimensions: i64 = storage
            .connection
            .query_row(
                "SELECT vec_length(embedding) FROM node_embeddings_vec WHERE embedding_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("read vector dimensions");
        assert_eq!(dimensions, 256);
    }

    #[test]
    fn version_three_user_data_survives_later_migrations() {
        register_sqlite_vec().expect("register sqlite-vec");
        let mut connection = Connection::open_in_memory().expect("open legacy database");
        configure_connection(&connection).expect("configure legacy database");
        connection
            .execute_batch(
                r#"CREATE TABLE schema_migrations (
                       version INTEGER PRIMARY KEY CHECK (version > 0),
                       name TEXT NOT NULL UNIQUE,
                       applied_at INTEGER NOT NULL
                   ) STRICT;"#,
            )
            .expect("create migration ledger");
        for migration in MIGRATIONS.iter().take(3) {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("start legacy migration");
            transaction
                .execute_batch(migration.sql)
                .expect("apply legacy migration");
            transaction
                .execute(
                    "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, 0)",
                    params![migration.version, migration.name],
                )
                .expect("record legacy migration");
            transaction
                .pragma_update(None, "user_version", migration.version)
                .expect("set legacy version");
            transaction.commit().expect("commit legacy migration");
        }
        connection
            .execute_batch(
                r#"INSERT INTO agents(id, metadata, created_at)
                   VALUES ('legacy-agent', '{}', 1);
                   INSERT INTO spaces(
                       id, kind, label, metadata, created_by_agent_id, created_at
                   ) VALUES (
                       'legacy-space', 'project', 'Legacy', '{}', 'legacy-agent', 2
                   );
                   INSERT INTO sessions(
                       id, space_id, agent_id, started_at, metadata
                   ) VALUES (
                       'legacy-session', 'legacy-space', 'legacy-agent', 3, '{}'
                   );
                   INSERT INTO nodes(
                       id, space_id, created_by_agent_id, created_in_session_id,
                       created_at, last_accessed, access_count, salience
                   ) VALUES (
                       'legacy-node', 'legacy-space', 'legacy-agent',
                       'legacy-session', 4, 4, 0, 0.8
                   );
                   INSERT INTO node_versions(
                       node_id, space_id, revision, kind, label, body, metadata,
                       valid_from, recorded_at, agent_id, session_id
                   ) VALUES (
                       'legacy-node', 'legacy-space', 1, 'memory', 'Legacy label',
                       'preserved body', '{}', 4, 4, 'legacy-agent', 'legacy-session'
                   );"#,
            )
            .expect("insert legacy user data");

        migrate(&mut connection).expect("upgrade legacy database");
        let preserved: (String, String) = connection
            .query_row(
                r#"SELECT version.body, node.salience_layer
                   FROM node_versions AS version
                   JOIN nodes AS node
                     ON node.space_id = version.space_id AND node.id = version.node_id
                   WHERE version.node_id = 'legacy-node'"#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read upgraded data");
        assert_eq!(preserved, ("preserved body".to_owned(), "hot".to_owned()));
        assert_eq!(
            read_schema_version(&connection).expect("schema version"),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn memory_listing_forget_and_status_preserve_bitemporal_history() {
        let storage = test_storage();
        storage
            .ensure_context(
                &Agent {
                    id: AGENT_ID.to_owned(),
                    display_name: Some("Test agent".to_owned()),
                    metadata_json: "{}".to_owned(),
                    created_at: 1,
                },
                &Space {
                    id: SPACE_ID.to_owned(),
                    kind: SpaceKind::Project,
                    label: "Test space".to_owned(),
                    metadata_json: "{}".to_owned(),
                    created_by_agent_id: AGENT_ID.to_owned(),
                    created_at: 2,
                },
                &Session {
                    id: SESSION_ID.to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    agent_id: AGENT_ID.to_owned(),
                    started_at: 3,
                    ended_at: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("ensure existing context idempotently");

        let mut memory = node(
            "owned-memory",
            "Remembered fact",
            "The release channel is stable",
            100,
            110,
        );
        memory.metadata_json =
            r#"{"brain_memory":true,"kind":"fact","tags":["release"]}"#.to_owned();
        storage.insert_node(&memory).expect("insert memory");

        let listed = storage
            .list_memories(SPACE_ID, Some(AGENT_ID), AsOf::new(150, 150), 10)
            .expect("list active memories");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "owned-memory");

        let status = storage
            .status(SPACE_ID, AsOf::new(150, 150))
            .expect("read status");
        assert_eq!(status.nodes, 1);
        assert_eq!(status.active_nodes, 1);
        assert_eq!(status.hot_nodes + status.warm_nodes + status.cold_nodes, 1);
        assert_eq!(status.latest_recorded_at, Some(110));

        storage
            .forget_memory(SPACE_ID, "owned-memory", AGENT_ID, SESSION_ID, 200, 201)
            .expect("forget memory");
        assert!(
            storage
                .list_memories(SPACE_ID, Some(AGENT_ID), AsOf::new(201, 201), 10,)
                .expect("list after forget")
                .is_empty()
        );
        assert!(
            storage
                .node_as_of(SPACE_ID, "owned-memory", AsOf::new(150, 199))
                .expect("read prior belief")
                .is_some()
        );
        assert!(
            storage
                .node_as_of(SPACE_ID, "owned-memory", AsOf::new(150, 201))
                .expect("read corrected historical fact")
                .is_some()
        );
        assert!(
            storage
                .node_as_of(SPACE_ID, "owned-memory", AsOf::new(201, 201))
                .expect("read forgotten current fact")
                .is_none()
        );

        let status = storage
            .status(SPACE_ID, AsOf::new(201, 201))
            .expect("read status after forget");
        assert_eq!(status.nodes, 1);
        assert_eq!(status.active_nodes, 0);
        assert_eq!(status.latest_recorded_at, Some(201));
    }

    #[test]
    fn replacement_preserves_old_fact_for_world_time_between_versions() {
        let storage = test_storage();
        storage
            .insert_node(&node(
                "runtime",
                "Runtime choice",
                "The runtime is version one",
                100,
                110,
            ))
            .expect("insert initial fact");

        storage
            .replace_node(
                SPACE_ID,
                "runtime",
                &NodeReplacement {
                    kind: NodeKind::Decision,
                    label: "Runtime choice".to_owned(),
                    body: "The runtime is version two".to_owned(),
                    source: None,
                    metadata_json: "{\"reason\":\"upgrade\"}".to_owned(),
                    valid_from: 200,
                    attribution: attribution(300),
                },
            )
            .expect("replace fact");

        let between = storage
            .node_as_of(SPACE_ID, "runtime", AsOf::new(150, 400))
            .expect("query old fact")
            .expect("old fact exists");
        assert_eq!(between.body, "The runtime is version one");
        assert_eq!(between.valid_to, Some(200));

        let after = storage
            .node_as_of(SPACE_ID, "runtime", AsOf::new(250, 400))
            .expect("query replacement")
            .expect("replacement exists");
        assert_eq!(after.body, "The runtime is version two");

        let knowledge_before_replacement = storage
            .node_as_of(SPACE_ID, "runtime", AsOf::new(250, 250))
            .expect("query prior knowledge")
            .expect("prior belief exists");
        assert_eq!(
            knowledge_before_replacement.body,
            "The runtime is version one"
        );
    }

    #[test]
    fn fts_search_uses_bm25_and_bitemporal_filters() {
        let storage = test_storage();
        storage
            .insert_node(&node(
                "alpha",
                "Compiler decision",
                "Use a compact compiler runtime",
                100,
                110,
            ))
            .expect("insert searchable node");
        storage
            .insert_node(&node(
                "beta",
                "Unrelated note",
                "A short status update",
                100,
                120,
            ))
            .expect("insert unrelated node");

        let hits = storage
            .search_nodes(SPACE_ID, "compiler", AsOf::new(150, 200), 10)
            .expect("search nodes");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node.id, "alpha");
        assert!(hits[0].bm25.is_finite());
    }

    #[test]
    fn composite_foreign_keys_prevent_cross_space_edges() {
        let storage = test_storage();
        storage
            .insert_node(&node("source", "Source", "Source node", 100, 110))
            .expect("insert source");

        storage
            .create_space(&Space {
                id: "other-space".to_owned(),
                kind: SpaceKind::Repository,
                label: "Other Space".to_owned(),
                metadata_json: "{}".to_owned(),
                created_by_agent_id: AGENT_ID.to_owned(),
                created_at: 40,
            })
            .expect("create other space");
        storage
            .create_session(&Session {
                id: "other-session".to_owned(),
                space_id: "other-space".to_owned(),
                agent_id: AGENT_ID.to_owned(),
                started_at: 50,
                ended_at: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("create other session");
        storage
            .insert_node(&NewNode {
                id: "target".to_owned(),
                space_id: "other-space".to_owned(),
                kind: NodeKind::Concept,
                label: "Target".to_owned(),
                body: "Target node".to_owned(),
                source: None,
                metadata_json: "{}".to_owned(),
                valid_from: 100,
                valid_to: None,
                attribution: Attribution {
                    agent_id: AGENT_ID.to_owned(),
                    session_id: "other-session".to_owned(),
                    recorded_at: 120,
                },
                salience: 0.5,
                source_weight: 0.5,
            })
            .expect("insert other target");

        let result = storage.insert_edge(&NewEdge {
            id: "cross-space".to_owned(),
            space_id: SPACE_ID.to_owned(),
            from_node_id: "source".to_owned(),
            to_node_id: "target".to_owned(),
            relation: "relates_to".to_owned(),
            weight: 0.4,
            confidence: 0.7,
            metadata_json: "{}".to_owned(),
            valid_from: 130,
            valid_to: None,
            attribution: attribution(140),
            salience: 0.5,
        });
        assert!(result.is_err());
    }

    #[test]
    fn four_concurrent_producers_do_not_lose_nodes() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let storage = test_storage();
        let barrier = Arc::new(Barrier::new(4));
        let mut producers = Vec::new();
        for producer in 0..4 {
            let storage = storage.clone();
            let barrier = Arc::clone(&barrier);
            producers.push(thread::spawn(move || {
                let nodes = (0..250)
                    .map(|index| {
                        node(
                            &format!("producer-{producer}-node-{index}"),
                            "Concurrent memory",
                            "Written through the single writer actor",
                            100,
                            200 + i64::from(producer),
                        )
                    })
                    .collect::<Vec<_>>();
                barrier.wait();
                storage.insert_nodes(&nodes).expect("insert producer batch");
            }));
        }
        for producer in producers {
            producer.join().expect("producer thread");
        }
        storage.flush().expect("flush writer");
        assert_eq!(storage.node_count(SPACE_ID).expect("count nodes"), 1_000);
    }

    #[test]
    fn maintenance_keeps_old_decision_hot_but_cools_session() {
        let storage = test_storage();
        let mut decision = node("decision", "Decision", "Durable rationale", 100, 100);
        decision.kind = NodeKind::Decision;
        decision.source_weight = 1.0;
        let mut session = node("session", "Session", "Routine interaction", 100, 100);
        session.kind = NodeKind::Session;
        session.source_weight = 1.0;
        storage
            .insert_nodes(&[decision, session])
            .expect("insert decay nodes");

        let now = 100 + 100 * DAY;
        storage
            .maintenance(now, 10, false)
            .expect("run maintenance");
        let decision = storage
            .node_as_of(SPACE_ID, "decision", AsOf::new(now, now))
            .expect("load decision")
            .expect("decision exists");
        let session = storage
            .node_as_of(SPACE_ID, "session", AsOf::new(now, now))
            .expect("load session")
            .expect("session exists");
        assert_eq!(decision.salience_layer, SalienceLayer::Hot);
        assert_ne!(session.salience_layer, SalienceLayer::Hot);
    }

    #[test]
    fn edge_insert_incrementally_materializes_two_hop_path() {
        let storage = test_storage();
        storage
            .insert_nodes(&[
                node("a", "A", "start", 100, 110),
                node("b", "B", "middle", 100, 111),
                node("c", "C", "end", 100, 112),
            ])
            .expect("insert path nodes");
        storage
            .insert_edge(&NewEdge {
                id: "ab".to_owned(),
                space_id: SPACE_ID.to_owned(),
                from_node_id: "a".to_owned(),
                to_node_id: "b".to_owned(),
                relation: "calls".to_owned(),
                weight: 0.8,
                confidence: 1.0,
                metadata_json: "{}".to_owned(),
                valid_from: 120,
                valid_to: None,
                attribution: attribution(120),
                salience: 1.0,
            })
            .expect("insert first edge");
        storage
            .insert_edge(&NewEdge {
                id: "bc".to_owned(),
                space_id: SPACE_ID.to_owned(),
                from_node_id: "b".to_owned(),
                to_node_id: "c".to_owned(),
                relation: "uses".to_owned(),
                weight: 0.9,
                confidence: 1.0,
                metadata_json: "{}".to_owned(),
                valid_from: 121,
                valid_to: None,
                attribution: attribution(121),
                salience: 1.0,
            })
            .expect("insert second edge");

        let paths = storage
            .materialized_paths_from(SPACE_ID, "a", 10)
            .expect("query paths");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].via_node_id, "b");
        assert_eq!(paths[0].to_node_id, "c");
        assert_eq!(paths[0].path_kind, "calls::uses");
        assert!((paths[0].path_weight - 0.72).abs() < f64::EPSILON);
    }

    fn snapshot(dir: &Path) -> Vec<(String, u64, std::time::SystemTime)> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                let entry = entry.expect("entry");
                let meta = entry.metadata().expect("metadata");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    meta.len(),
                    meta.modified().expect("mtime"),
                )
            })
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn read_only_open_never_creates_migrates_or_writes() {
        let dir = std::env::temp_dir().join(format!("brain-ro-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");

        let missing = dir.join("missing.sqlite");
        assert!(matches!(
            SqliteStorage::open_read_only(&missing),
            Err(StorageError::NotFound(_))
        ));
        assert!(
            snapshot(&dir).is_empty(),
            "a missing database stays missing"
        );

        let legacy = dir.join("legacy.sqlite");
        Connection::open(&legacy)
            .expect("legacy")
            .execute_batch("CREATE TABLE t(x); PRAGMA user_version = 0;")
            .expect("legacy schema");
        let before = snapshot(&dir);
        assert!(matches!(
            SqliteStorage::open_read_only(&legacy),
            Err(StorageError::InvalidData(_))
        ));
        assert_eq!(
            snapshot(&dir),
            before,
            "an old schema is refused, not migrated"
        );

        let current = dir.join("current.sqlite");
        drop(SqliteStorage::open(&current).expect("create current"));
        let main_file = |dir: &Path| {
            snapshot(dir)
                .into_iter()
                .filter(|(name, _, _)| !name.ends_with("-wal") && !name.ends_with("-shm"))
                .collect::<Vec<_>>()
        };
        let before = main_file(&dir);
        {
            let storage = SqliteStorage::open_read_only(&current).expect("read-only open");
            assert_eq!(storage.schema_version().expect("version"), SCHEMA_VERSION);
            assert!(
                storage
                    .record_node_accesses(SPACE_ID, &["n".to_owned()], 1)
                    .is_err(),
                "writes fail on a read-only handle"
            );
        }
        // SQLite may leave its -wal/-shm coordination files; the database
        // file itself (size, mtime) and every other file stay untouched.
        assert_eq!(main_file(&dir), before, "no application data change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Starts with no WAL sidecars, then a writer appears and keeps inserting
    /// and checkpointing. The read-only reader must follow it through normal
    /// SQLite locking: every read is internally consistent, never goes
    /// backwards, and the final rows (left in the WAL, after the last
    /// checkpoint) are visible — an unlocked immutable open would miss them.
    #[test]
    fn read_only_reader_follows_a_live_writer_with_checkpoints() {
        let dir = std::env::temp_dir().join(format!("brain-ro-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("live.sqlite");
        drop(SqliteStorage::open(&path).expect("create"));
        {
            let setup = Connection::open(&path).expect("setup");
            setup
                .execute_batch(
                    "CREATE TABLE probe(x INTEGER PRIMARY KEY, double INTEGER NOT NULL);",
                )
                .expect("probe table");
        }
        let sidecars = |p: &Path| {
            ["-wal", "-shm"].iter().any(|suffix| {
                let mut name = p.as_os_str().to_owned();
                name.push(suffix);
                Path::new(&name).exists()
            })
        };
        assert!(!sidecars(&path), "starts without WAL sidecars");

        let reader = SqliteDatabase::open_read_only(&path).expect("read-only open");
        const ROWS: i64 = 300;
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let writer = Connection::open(&writer_path).expect("writer");
            writer.busy_timeout(Duration::from_secs(5)).expect("busy");
            for x in 1..=ROWS {
                writer
                    .execute(
                        "INSERT INTO probe(x, double) VALUES (?1, ?2)",
                        params![x, x * 2],
                    )
                    .expect("insert");
                if x % 25 == 0 && x < ROWS {
                    let _: (i64, i64, i64) = writer
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .expect("checkpoint");
                }
            }
        });
        let read = || -> (i64, i64, i64) {
            reader
                .connection
                .query_row(
                    "SELECT count(*), coalesce(max(x), 0), coalesce(sum(double), 0) FROM probe",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .expect("consistent read")
        };
        let mut last = 0;
        let mut reads = 0;
        while !writer.is_finished() {
            let (count, max, sum) = read();
            assert_eq!(count, max, "a read sees a whole prefix of commits");
            assert_eq!(sum, count * (count + 1), "rows are complete, never torn");
            assert!(count >= last, "reads never go backwards");
            last = count;
            reads += 1;
        }
        writer.join().expect("writer");
        assert!(reads > 0);
        assert_eq!(
            read(),
            (ROWS, ROWS, ROWS * (ROWS + 1)),
            "WAL-only tail visible"
        );
        assert_eq!(verify_current_schema(&reader.connection).ok(), Some(()));
        assert!(
            reader.connection.execute("DELETE FROM probe", []).is_err(),
            "query_only refuses writes"
        );
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
