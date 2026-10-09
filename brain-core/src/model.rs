//! Storage-neutral domain records.

use std::fmt;

/// Unix time in seconds.
pub type Timestamp = i64;

/// Supported node categories.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NodeKind {
    /// Source code or a code symbol.
    Code,
    /// A document or document fragment.
    Document,
    /// A durable memory or fact.
    Memory,
    /// An agent work session.
    Session,
    /// A decision and its rationale.
    Decision,
    /// An error, failure, or diagnostic.
    Error,
    /// An abstract concept.
    Concept,
    /// A concrete named entity.
    Entity,
}

impl NodeKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Document => "document",
            Self::Memory => "memory",
            Self::Session => "session",
            Self::Decision => "decision",
            Self::Error => "error",
            Self::Concept => "concept",
            Self::Entity => "entity",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "code" => Some(Self::Code),
            "document" => Some(Self::Document),
            "memory" => Some(Self::Memory),
            "session" => Some(Self::Session),
            "decision" => Some(Self::Decision),
            "error" => Some(Self::Error),
            "concept" => Some(Self::Concept),
            "entity" => Some(Self::Entity),
            _ => None,
        }
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Search/retention layer derived from a node's salience.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SalienceLayer {
    /// Included in ordinary search.
    Hot,
    /// Included only when callers explicitly request the warm layer.
    Warm,
    /// Archived outside ordinary search.
    Cold,
}

impl SalienceLayer {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }

    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        match value {
            "hot" => Some(Self::Hot),
            "warm" => Some(Self::Warm),
            "cold" => Some(Self::Cold),
            _ => None,
        }
    }
}

impl fmt::Display for SalienceLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Isolation category for a memory space.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SpaceKind {
    /// A project that may span repositories.
    Project,
    /// One source repository.
    Repository,
    /// Personal memory not tied to a project.
    Personal,
    /// An application-defined space.
    Custom,
}

impl SpaceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Repository => "repository",
            Self::Personal => "personal",
            Self::Custom => "custom",
        }
    }
}

impl fmt::Display for SpaceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Agent identity stored independently of any particular client product.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agent {
    /// Stable, caller-provided agent identifier.
    pub id: String,
    /// Optional human-readable name.
    pub display_name: Option<String>,
    /// JSON object with application-defined metadata.
    pub metadata_json: String,
    /// Time the identity was registered.
    pub created_at: Timestamp,
}

/// An isolated memory space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Space {
    /// Stable, caller-provided identifier.
    pub id: String,
    /// Isolation category.
    pub kind: SpaceKind,
    /// Human-readable label.
    pub label: String,
    /// JSON object with application-defined metadata.
    pub metadata_json: String,
    /// Agent that created the space.
    pub created_by_agent_id: String,
    /// Creation time.
    pub created_at: Timestamp,
}

/// A work session attributed to exactly one agent and one space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    /// Stable, caller-provided identifier.
    pub id: String,
    /// Owning memory space.
    pub space_id: String,
    /// Agent running the session.
    pub agent_id: String,
    /// Session start time.
    pub started_at: Timestamp,
    /// Optional session end time.
    pub ended_at: Option<Timestamp>,
    /// JSON object with application-defined metadata.
    pub metadata_json: String,
}

/// Authorship and system-time attribution for a version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attribution {
    /// Agent that recorded the version.
    pub agent_id: String,
    /// Session in which the version was recorded.
    pub session_id: String,
    /// System time at which the version became known.
    pub recorded_at: Timestamp,
}

/// The two coordinates needed for a bi-temporal query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AsOf {
    /// Time in the outside world.
    pub valid_at: Timestamp,
    /// Time in the database's knowledge history.
    pub recorded_at: Timestamp,
}

impl AsOf {
    /// Creates a bi-temporal query point.
    #[must_use]
    pub const fn new(valid_at: Timestamp, recorded_at: Timestamp) -> Self {
        Self {
            valid_at,
            recorded_at,
        }
    }
}

/// Input for the first version of a logical node.
#[derive(Clone, Debug, PartialEq)]
pub struct NewNode {
    /// Stable logical identifier shared by all versions.
    pub id: String,
    /// Owning memory space.
    pub space_id: String,
    /// Node category.
    pub kind: NodeKind,
    /// Short searchable label.
    pub label: String,
    /// Full searchable content.
    pub body: String,
    /// Optional source path or URL.
    pub source: Option<String>,
    /// JSON object with application-defined metadata.
    pub metadata_json: String,
    /// First instant at which the fact is valid.
    pub valid_from: Timestamp,
    /// Exclusive end of the fact's validity, if known.
    pub valid_to: Option<Timestamp>,
    /// Authorship and recording time.
    pub attribution: Attribution,
    /// Initial decay salience in the inclusive range 0 through 1.
    pub salience: f64,
    /// Reliability of the source in the inclusive range 0 through 1.
    pub source_weight: f64,
}

/// Replacement content for a logical node.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeReplacement {
    /// Replacement node category.
    pub kind: NodeKind,
    /// Replacement label.
    pub label: String,
    /// Replacement body.
    pub body: String,
    /// Replacement source path or URL.
    pub source: Option<String>,
    /// Replacement JSON metadata object.
    pub metadata_json: String,
    /// World time at which the replacement becomes valid.
    pub valid_from: Timestamp,
    /// Authorship and system time at which the replacement was learned.
    pub attribution: Attribution,
}

/// One bi-temporal view of a logical node.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable logical identifier.
    pub id: String,
    /// Physical version row identifier.
    pub version_id: i64,
    /// Monotonic version number within the logical node.
    pub revision: i64,
    /// Owning memory space.
    pub space_id: String,
    /// Node category at this point in time.
    pub kind: NodeKind,
    /// Searchable label.
    pub label: String,
    /// Searchable body.
    pub body: String,
    /// Optional source path or URL.
    pub source: Option<String>,
    /// JSON metadata object.
    pub metadata_json: String,
    /// Inclusive start of world-time validity.
    pub valid_from: Timestamp,
    /// Exclusive end of world-time validity.
    pub valid_to: Option<Timestamp>,
    /// Time this assertion entered the database.
    pub recorded_at: Timestamp,
    /// Exclusive end of system-time validity.
    pub superseded_at: Option<Timestamp>,
    /// Agent that recorded this assertion.
    pub agent_id: String,
    /// Session that recorded this assertion.
    pub session_id: String,
    /// Most recent access time for decay calculations.
    pub last_accessed: Timestamp,
    /// Number of accesses recorded for the logical node.
    pub access_count: i64,
    /// Current decay salience in the inclusive range 0 through 1.
    pub salience: f64,
    /// Time the stable logical node was first recorded.
    pub created_at: Timestamp,
    /// Reliability of the node's source.
    pub source_weight: f64,
    /// Number of incident edges used by the structural salience term.
    pub degree: i64,
    /// Persisted salience layer.
    pub salience_layer: SalienceLayer,
    /// Estimated content size used for hard search budgets.
    pub token_len: usize,
    /// Next scheduled salience recalculation.
    pub next_salience_at: Timestamp,
}

/// Input for the first version of a logical edge.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEdge {
    /// Stable logical edge identifier.
    pub id: String,
    /// Owning memory space.
    pub space_id: String,
    /// Source logical node identifier.
    pub from_node_id: String,
    /// Target logical node identifier.
    pub to_node_id: String,
    /// Relationship name such as `calls`, `mentions`, or `relates_to`.
    pub relation: String,
    /// Trust weight in the inclusive range 0 through 1.
    pub weight: f64,
    /// Extraction confidence in the inclusive range 0 through 1.
    pub confidence: f64,
    /// JSON object with application-defined metadata.
    pub metadata_json: String,
    /// Inclusive start of world-time validity.
    pub valid_from: Timestamp,
    /// Exclusive end of world-time validity.
    pub valid_to: Option<Timestamp>,
    /// Authorship and recording time.
    pub attribution: Attribution,
    /// Initial decay salience in the inclusive range 0 through 1.
    pub salience: f64,
}

/// One bi-temporal view of a logical edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// Stable logical edge identifier.
    pub id: String,
    /// Physical version row identifier.
    pub version_id: i64,
    /// Monotonic version number within the logical edge.
    pub revision: i64,
    /// Owning memory space.
    pub space_id: String,
    /// Source logical node identifier.
    pub from_node_id: String,
    /// Target logical node identifier.
    pub to_node_id: String,
    /// Relationship name.
    pub relation: String,
    /// Trust weight.
    pub weight: f64,
    /// Extraction confidence.
    pub confidence: f64,
    /// JSON metadata object.
    pub metadata_json: String,
    /// Inclusive start of world-time validity.
    pub valid_from: Timestamp,
    /// Exclusive end of world-time validity.
    pub valid_to: Option<Timestamp>,
    /// Time this assertion entered the database.
    pub recorded_at: Timestamp,
    /// Exclusive end of system-time validity.
    pub superseded_at: Option<Timestamp>,
    /// Agent that recorded this assertion.
    pub agent_id: String,
    /// Session that recorded this assertion.
    pub session_id: String,
    /// Most recent access time for decay calculations.
    pub last_accessed: Timestamp,
    /// Number of recorded accesses.
    pub access_count: i64,
    /// Current decay salience.
    pub salience: f64,
}

/// A full-text result ordered by SQLite's BM25 score.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchHit {
    /// Matching bi-temporal node view.
    pub node: Node,
    /// Native FTS5 BM25 score; lower values rank first.
    pub bm25: f64,
}

/// Salience layers eligible for a candidate query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchLayers {
    /// Include hot nodes.
    pub hot: bool,
    /// Include warm nodes.
    pub warm: bool,
    /// Include cold nodes.
    pub cold: bool,
}

impl SearchLayers {
    /// Ordinary product search: hot nodes only.
    pub const HOT: Self = Self {
        hot: true,
        warm: false,
        cold: false,
    };

    /// Explicit expanded search over hot and warm nodes.
    pub const HOT_AND_WARM: Self = Self {
        hot: true,
        warm: true,
        cold: false,
    };

    /// Administrative search over every layer.
    pub const ALL: Self = Self {
        hot: true,
        warm: true,
        cold: true,
    };
}

impl Default for SearchLayers {
    fn default() -> Self {
        Self::HOT
    }
}

/// Vector-search candidate returned by a storage backend.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorSearchHit {
    /// Matching bi-temporal node view.
    pub node: Node,
    /// Cosine similarity in the range -1 through 1.
    pub cosine: f64,
    /// Stored vector used for semantic deduplication.
    pub embedding: Vec<f32>,
}

/// Fully fused result returned by hybrid search.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridSearchHit {
    /// Matching node.
    pub node: Node,
    /// Reciprocal-rank-fusion score plus any exact-match bonus.
    pub score: f64,
    /// Whether BM25 contributed a ranked signal.
    pub matched_bm25: bool,
    /// Whether vector similarity contributed a ranked signal.
    pub matched_vector: bool,
    /// Whether exact entity matching contributed its bonus.
    pub matched_exact: bool,
    /// Hops away from a directly matched record, when the graph walk reached it.
    ///
    /// `None` means the record matched the query itself. A value means it was
    /// pulled in because it is connected to something that did, which is how a
    /// question finds records that share no words with it.
    pub graph_distance: Option<u32>,
}

/// Results together with the exact token budget consumed.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchOutput {
    /// Relevance-ordered, deduplicated hits.
    pub hits: Vec<HybridSearchHit>,
    /// Sum of persisted token estimates for all returned hits.
    pub used_tokens: usize,
    /// Whether a vector signal contributed candidates to this query.
    pub vector_search: bool,
    /// Records returned because of graph evidence, after relevance filtering,
    /// deduplication, and the token budget.
    ///
    /// Reported so the graph-depth setting is visibly doing something. It was
    /// possible to move that slider for months and change nothing at all.
    pub graph_reached: usize,
    /// Honest degradation notices, such as a missing local embedding model.
    pub warnings: Vec<String>,
}

/// Input for an embedding attached to one immutable node version.
#[derive(Clone, Debug, PartialEq)]
pub struct NewEmbedding {
    /// Physical node-version row identifier.
    pub node_version_id: i64,
    /// Owning memory space.
    pub space_id: String,
    /// Stable backend/model identifier.
    pub model_id: String,
    /// Dense vector payload.
    pub vector: Vec<f32>,
    /// Time the vector was produced.
    pub embedded_at: Timestamp,
    /// Authorship of the embedding operation.
    pub attribution: Attribution,
}

/// One precomputed two-hop path.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializedPath {
    /// Owning memory space.
    pub space_id: String,
    /// Start node.
    pub from_node_id: String,
    /// Intermediate node.
    pub via_node_id: String,
    /// End node.
    pub to_node_id: String,
    /// First edge identifier.
    pub first_edge_id: String,
    /// Second edge identifier.
    pub second_edge_id: String,
    /// Product of the two edge weights.
    pub path_weight: f64,
    /// Concatenated relation names.
    pub path_kind: String,
    /// Last incremental refresh time.
    pub updated_at: Timestamp,
}

/// One live edge reached from a requested graph frontier node.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNeighbor {
    /// Frontier node whose adjacency list produced this result.
    pub source_node_id: String,
    /// Node at the other end of the active edge.
    pub node_id: String,
    /// Highest weight when several active edges join the same pair.
    pub edge_weight: f64,
}

/// Work completed by one maintenance tick.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceReport {
    /// Nodes whose salience and schedule were refreshed.
    pub salience_nodes: usize,
    /// Deferred edges whose materialized paths were refreshed.
    pub path_edges: usize,
    /// Whether this tick ran `VACUUM`.
    pub vacuumed: bool,
}

/// Aggregate database state for one isolated memory space.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageStatus {
    /// Stable logical nodes, including records no longer valid now.
    pub nodes: usize,
    /// Nodes visible at the requested bi-temporal point.
    pub active_nodes: usize,
    /// Stable logical edges, including records no longer valid now.
    pub edges: usize,
    /// Edges visible at the requested bi-temporal point.
    pub active_edges: usize,
    /// Active nodes in the hot salience layer.
    pub hot_nodes: usize,
    /// Active nodes in the warm salience layer.
    pub warm_nodes: usize,
    /// Active nodes in the cold salience layer.
    pub cold_nodes: usize,
    /// Persisted embedding records for this space.
    pub embeddings: usize,
    /// Most recent system-time assertion recorded in this space.
    pub latest_recorded_at: Option<Timestamp>,
}
