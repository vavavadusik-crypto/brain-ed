//! Core types and storage abstractions for Brain for AI.

pub mod coordination;
pub mod embeddings;
mod error;
pub mod goals;
mod memory;
mod model;
pub mod network;
pub mod operations;
pub mod profile;
pub mod salience;
mod search;
mod storage;
pub mod templates;
pub mod temporal;

pub use error::{StorageError, StorageResult};
pub use memory::{
    MemoryContradiction, MemoryDuplicate, MemoryError, MemoryKind, MemoryRecord, MemoryService,
    RememberOutput, RememberRequest,
};
pub use model::{
    Agent, AsOf, Attribution, Edge, GraphNeighbor, HybridSearchHit, MaintenanceReport,
    MaterializedPath, NewEdge, NewEmbedding, NewNode, Node, NodeKind, NodeReplacement,
    SalienceLayer, SearchHit, SearchLayers, SearchOutput, Session, Space, SpaceKind, StorageStatus,
    Timestamp, VectorSearchHit,
};
pub use search::{HybridSearch, SearchError, SearchQuery};
pub use storage::{SCHEMA_VERSION, SqliteStorage, Storage};
pub mod indexer;
pub mod map;
pub mod sharing;

pub use sharing::{
    CursorState, EVENT_FORMAT, EventKind, FORBIDDEN_PAYLOAD_KEYS, FactKind, FactRow, FactStatus,
    LOCAL_PEER, MAX_BATCH_BYTES, MAX_BATCH_EVENTS, MAX_CLAIM_BYTES, MAX_FACT_ID_BYTES,
    MAX_ID_BYTES, MAX_PAGE, MAX_PAYLOAD_BYTES, MAX_SEQ, Principal, READ_GRANT_PREFIX,
    SHARE_APPLICATION_ID, SHARE_SCHEMA_VERSION, ShareError, ShareEvent, ShareResult, ShareStore,
    SharedFact, TrustedPeer,
};
