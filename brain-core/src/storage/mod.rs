//! Storage abstraction and built-in SQLite implementation.

mod actor;
mod migrations;
mod sqlite;

use crate::{
    Agent, AsOf, Edge, GraphNeighbor, MaintenanceReport, MaterializedPath, NewEdge, NewEmbedding,
    NewNode, Node, NodeReplacement, SearchHit, SearchLayers, Session, Space, StorageResult,
    StorageStatus, Timestamp, VectorSearchHit,
};

pub use actor::SqliteStorage;
pub use migrations::SCHEMA_VERSION;

/// Persistence boundary used by the memory engine.
///
/// Domain code depends on this trait rather than on SQLite-specific types, so
/// another storage engine can be introduced without rewriting the core.
pub trait Storage: Send + Sync {
    /// Returns the highest migration applied to this database.
    ///
    /// # Errors
    ///
    /// Returns an error when the version metadata cannot be read.
    fn schema_version(&self) -> StorageResult<u32>;

    /// Registers an agent identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or a duplicate identifier.
    fn create_agent(&self, agent: &Agent) -> StorageResult<()>;

    /// Creates an isolated memory space.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata or missing agent attribution.
    fn create_space(&self, space: &Space) -> StorageResult<()>;

    /// Opens an attributed agent session in a space.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid metadata, time windows, or foreign keys.
    fn create_session(&self, session: &Session) -> StorageResult<()>;

    /// Creates any missing agent, space, and session rows as one idempotent write.
    ///
    /// # Errors
    ///
    /// Returns an error when the supplied identities disagree or persistence fails.
    fn ensure_context(&self, agent: &Agent, space: &Space, session: &Session) -> StorageResult<()>;

    /// Inserts the first version of a logical node.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, attribution, or persistence fails.
    fn insert_node(&self, node: &NewNode) -> StorageResult<()>;

    /// Inserts many nodes as one actor command and one savepoint.
    ///
    /// # Errors
    ///
    /// Returns an error when any node is invalid or cannot be persisted.
    fn insert_nodes(&self, nodes: &[NewNode]) -> StorageResult<()>;

    /// Replaces the version valid at `replacement.valid_from` without deleting history.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no active fact at that world time, the
    /// recording time does not move forward, or persistence fails.
    fn replace_node(
        &self,
        space_id: &str,
        node_id: &str,
        replacement: &NodeReplacement,
    ) -> StorageResult<()>;

    /// Reads a node at one world-time and one system-time coordinate using one SQL query.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or stored-value decoding fails.
    fn node_as_of(&self, space_id: &str, node_id: &str, as_of: AsOf)
    -> StorageResult<Option<Node>>;

    /// Lists active user-memory nodes, optionally restricted to their creating agent.
    ///
    /// # Errors
    ///
    /// Returns an error when stored data cannot be decoded or the query fails.
    fn list_memories(
        &self,
        space_id: &str,
        created_by_agent_id: Option<&str>,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<Node>>;

    /// Closes a user-memory validity window without deleting its history.
    ///
    /// # Errors
    ///
    /// Returns an error when the node is not an active memory owned by the agent,
    /// temporal coordinates do not advance, or persistence fails.
    fn forget_memory(
        &self,
        space_id: &str,
        node_id: &str,
        agent_id: &str,
        session_id: &str,
        valid_to: Timestamp,
        recorded_at: Timestamp,
    ) -> StorageResult<()>;

    /// Searches node text with FTS5/BM25 at a bi-temporal query point.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid FTS syntax, stored data, or database access.
    fn search_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>>;

    /// Searches BM25 candidates restricted to selected salience layers.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid FTS syntax, data decoding, or database access.
    fn search_nodes_in_layers(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>>;

    /// Searches the statically linked vector index within one memory space/model.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported dimensions, corrupt vectors, or database access.
    fn vector_search(
        &self,
        space_id: &str,
        model_id: &str,
        query_vector: &[f32],
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<VectorSearchHit>>;

    /// Finds exact labels, entity mentions, and logical identifiers in a query.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or stored-value decoding fails.
    fn exact_match_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<Node>>;

    /// Inserts the first version of a logical edge.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, attribution, or persistence fails.
    fn insert_edge(&self, edge: &NewEdge) -> StorageResult<()>;

    /// Inserts an initial graph in bulk and materializes its two-hop paths.
    ///
    /// # Errors
    ///
    /// Returns an error when any edge is invalid or cannot be persisted.
    fn insert_edges(&self, edges: &[NewEdge]) -> StorageResult<()>;

    /// Reads an edge at one world-time and one system-time coordinate using one SQL query.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or stored-value decoding fails.
    fn edge_as_of(&self, space_id: &str, edge_id: &str, as_of: AsOf)
    -> StorageResult<Option<Edge>>;

    /// Stores or replaces an embedding for one immutable node version.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid attribution, dimensions, values, or storage.
    fn upsert_embedding(&self, embedding: &NewEmbedding) -> StorageResult<()>;

    /// Stores many embeddings as one actor command and one savepoint.
    ///
    /// # Errors
    ///
    /// Returns an error when any embedding is invalid or cannot be persisted.
    fn upsert_embeddings(&self, embeddings: &[NewEmbedding]) -> StorageResult<()>;

    /// Records a node access and immediately refreshes salience/layer scheduling.
    ///
    /// # Errors
    ///
    /// Returns an error when the node does not exist or the actor cannot commit.
    fn record_node_access(
        &self,
        space_id: &str,
        node_id: &str,
        now: Timestamp,
    ) -> StorageResult<()>;

    /// Records one search access for several nodes in a single actor write.
    ///
    /// # Errors
    ///
    /// Returns an error when a node does not exist or the batch cannot commit.
    fn record_node_accesses(
        &self,
        space_id: &str,
        node_ids: &[String],
        now: Timestamp,
    ) -> StorageResult<()>;

    /// Returns the highest-weight materialized two-hop paths from one node.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or stored-value decoding fails.
    fn materialized_paths_from(
        &self,
        space_id: &str,
        node_id: &str,
        limit: usize,
    ) -> StorageResult<Vec<MaterializedPath>>;

    /// Counts precomputed two-hop paths in one space.
    ///
    /// # Errors
    ///
    /// Returns an error when the count query fails.
    fn materialized_path_count(&self, space_id: &str) -> StorageResult<usize>;

    /// Whether the space holds any edge at all.
    ///
    /// Distinguishes "the graph has connections this query did not reach" from
    /// "there is no graph yet", which read identically to the walk but mean
    /// opposite things to the person asking.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    fn has_edges(&self, space_id: &str) -> StorageResult<bool>;

    /// Returns the nodes one active edge away at a bi-temporal coordinate, in
    /// either direction, heaviest first.
    ///
    /// Materialized paths only cover two hops, so they cannot answer "what is
    /// directly connected to this" — a neighbour with no outgoing edge of its
    /// own appears in no path at all.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    fn graph_neighbors(
        &self,
        space_id: &str,
        source_node_ids: &[String],
        as_of: AsOf,
        layers: SearchLayers,
        memory_only: bool,
        limit_per_source: usize,
    ) -> StorageResult<Vec<GraphNeighbor>>;

    /// Rebuilds every two-hop path in one space from the edges that exist now.
    ///
    /// Paths are normally maintained incrementally as edges arrive, which leaves
    /// nothing to rebuild — except after an import that wrote edges by another
    /// route. A database migrated from an older store arrived with 31,131 edges
    /// and no paths at all, and nothing reported it.
    ///
    /// # Errors
    ///
    /// Returns an error when the rebuild query fails.
    fn rebuild_paths(&self, space_id: &str) -> StorageResult<usize>;

    /// Runs one incremental salience/path maintenance tick and optional vacuum.
    ///
    /// # Errors
    ///
    /// Returns an error if recalculation, path refresh, or vacuuming fails.
    fn maintenance(
        &self,
        now: Timestamp,
        batch_size: usize,
        allow_vacuum: bool,
    ) -> StorageResult<MaintenanceReport>;

    /// Returns the number of logical nodes in a memory space.
    ///
    /// # Errors
    ///
    /// Returns an error when the count query fails.
    fn node_count(&self, space_id: &str) -> StorageResult<usize>;

    /// Returns counts, salience layers, embeddings, and freshness for one space.
    ///
    /// # Errors
    ///
    /// Returns an error when the aggregate query fails or stored counts are invalid.
    fn status(&self, space_id: &str, as_of: AsOf) -> StorageResult<StorageStatus>;

    /// Waits until every write submitted before this call has committed.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped.
    fn flush(&self) -> StorageResult<()>;
}
