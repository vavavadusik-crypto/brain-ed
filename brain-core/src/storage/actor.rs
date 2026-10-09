//! Single-writer SQLite actor and synchronous storage facade.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Storage;
use super::sqlite::{SqliteDatabase, WriteOperation};
use crate::{
    Agent, AsOf, Edge, GraphNeighbor, MaintenanceReport, MaterializedPath, NewEdge, NewEmbedding,
    NewNode, Node, NodeReplacement, SearchHit, SearchLayers, Session, Space, StorageError,
    StorageResult, StorageStatus, Timestamp, VectorSearchHit,
};

const WRITE_BATCH_INTERVAL: Duration = Duration::from_millis(75);
const MAX_WRITE_BATCH: usize = 1_024;
const BACKGROUND_TICK_INTERVAL: Duration = Duration::from_secs(60);
const BACKGROUND_BATCH_SIZE: usize = 256;

type Reply<T> = mpsc::SyncSender<StorageResult<T>>;

enum Command {
    Write(WriteOperation, Reply<()>),
    SchemaVersion(Reply<u32>),
    NodeAsOf {
        space_id: String,
        node_id: String,
        as_of: AsOf,
        reply: Reply<Option<Node>>,
    },
    ListMemories {
        space_id: String,
        created_by_agent_id: Option<String>,
        as_of: AsOf,
        limit: usize,
        reply: Reply<Vec<Node>>,
    },
    SearchNodes {
        space_id: String,
        query: String,
        as_of: AsOf,
        limit: usize,
        reply: Reply<Vec<SearchHit>>,
    },
    SearchNodesInLayers {
        space_id: String,
        query: String,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
        reply: Reply<Vec<SearchHit>>,
    },
    VectorSearch {
        space_id: String,
        model_id: String,
        query_vector: Vec<f32>,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
        reply: Reply<Vec<VectorSearchHit>>,
    },
    ExactMatchNodes {
        space_id: String,
        query: String,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
        reply: Reply<Vec<Node>>,
    },
    EdgeAsOf {
        space_id: String,
        edge_id: String,
        as_of: AsOf,
        reply: Reply<Option<Edge>>,
    },
    MaterializedPaths {
        space_id: String,
        node_id: String,
        limit: usize,
        reply: Reply<Vec<MaterializedPath>>,
    },
    MaterializedPathCount {
        space_id: String,
        reply: Reply<usize>,
    },
    HasEdges {
        space_id: String,
        reply: Reply<bool>,
    },
    RebuildPaths {
        space_id: String,
        reply: Reply<usize>,
    },
    GraphNeighbors {
        space_id: String,
        source_node_ids: Vec<String>,
        as_of: AsOf,
        layers: SearchLayers,
        memory_only: bool,
        limit_per_source: usize,
        reply: Reply<Vec<GraphNeighbor>>,
    },
    Maintenance {
        now: Timestamp,
        batch_size: usize,
        allow_vacuum: bool,
        reply: Reply<MaintenanceReport>,
    },
    NodeCount {
        space_id: String,
        reply: Reply<usize>,
    },
    Status {
        space_id: String,
        as_of: AsOf,
        reply: Reply<StorageStatus>,
    },
    Flush(Reply<()>),
    Shutdown,
}

struct ActorHandle {
    sender: mpsc::Sender<Command>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for ActorHandle {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
        if let Ok(thread) = self.thread.get_mut()
            && let Some(thread) = thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Cloneable facade whose SQLite connection lives exclusively on one actor thread.
#[derive(Clone)]
pub struct SqliteStorage {
    actor: Arc<ActorHandle>,
}

impl SqliteStorage {
    /// Opens or creates a database and applies all pending migrations.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor cannot open, configure, or migrate SQLite.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::spawn(Some(path.as_ref().to_path_buf()))
    }

    /// Opens a migrated in-memory database owned by the writer actor.
    ///
    /// # Errors
    ///
    /// Returns an error when SQLite configuration or migration fails.
    pub fn open_in_memory() -> StorageResult<Self> {
        Self::spawn(None)
    }

    /// Opens an existing database at the current schema for reading only.
    /// Nothing is created or migrated; every write operation fails.
    ///
    /// # Errors
    ///
    /// `NotFound` when the file is missing; `InvalidData` when its schema is
    /// not exactly the current one; SQLite errors otherwise.
    pub fn open_read_only(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::spawn_with(Some((path.as_ref().to_path_buf(), true)))
    }

    fn spawn(path: Option<PathBuf>) -> StorageResult<Self> {
        Self::spawn_with(path.map(|path| (path, false)))
    }

    fn spawn_with(path: Option<(PathBuf, bool)>) -> StorageResult<Self> {
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("brain-sqlite-writer".to_owned())
            .spawn(move || {
                let database = match path {
                    Some((path, true)) => SqliteDatabase::open_read_only(path),
                    Some((path, false)) => SqliteDatabase::open(path),
                    None => SqliteDatabase::open_in_memory(),
                };
                match database {
                    Ok(database) => {
                        let _ = ready_sender.send(Ok(()));
                        actor_loop(database, receiver);
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                    }
                }
            })
            .map_err(|error| StorageError::ActorUnavailable(error.to_string()))?;

        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                actor: Arc::new(ActorHandle {
                    sender,
                    thread: Mutex::new(Some(thread)),
                }),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(error) => {
                let _ = thread.join();
                Err(StorageError::ActorUnavailable(error.to_string()))
            }
        }
    }

    fn write(&self, operation: WriteOperation) -> StorageResult<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Write(operation, reply))?;
        receive(result)
    }

    fn send(&self, command: Command) -> StorageResult<()> {
        self.actor
            .sender
            .send(command)
            .map_err(|error| StorageError::ActorUnavailable(error.to_string()))
    }
}

impl Storage for SqliteStorage {
    fn schema_version(&self) -> StorageResult<u32> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::SchemaVersion(reply))?;
        receive(result)
    }

    fn create_agent(&self, agent: &Agent) -> StorageResult<()> {
        self.write(WriteOperation::CreateAgent(agent.clone()))
    }

    fn create_space(&self, space: &Space) -> StorageResult<()> {
        self.write(WriteOperation::CreateSpace(space.clone()))
    }

    fn create_session(&self, session: &Session) -> StorageResult<()> {
        self.write(WriteOperation::CreateSession(session.clone()))
    }

    fn ensure_context(&self, agent: &Agent, space: &Space, session: &Session) -> StorageResult<()> {
        self.write(WriteOperation::EnsureContext {
            agent: Box::new(agent.clone()),
            space: Box::new(space.clone()),
            session: Box::new(session.clone()),
        })
    }

    fn insert_node(&self, node: &NewNode) -> StorageResult<()> {
        self.write(WriteOperation::InsertNodes(vec![node.clone()]))
    }

    fn insert_nodes(&self, nodes: &[NewNode]) -> StorageResult<()> {
        self.write(WriteOperation::InsertNodes(nodes.to_vec()))
    }

    fn replace_node(
        &self,
        space_id: &str,
        node_id: &str,
        replacement: &NodeReplacement,
    ) -> StorageResult<()> {
        self.write(WriteOperation::ReplaceNode {
            space_id: space_id.to_owned(),
            node_id: node_id.to_owned(),
            replacement: replacement.clone(),
        })
    }

    fn node_as_of(
        &self,
        space_id: &str,
        node_id: &str,
        as_of: AsOf,
    ) -> StorageResult<Option<Node>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::NodeAsOf {
            space_id: space_id.to_owned(),
            node_id: node_id.to_owned(),
            as_of,
            reply,
        })?;
        receive(result)
    }

    fn list_memories(
        &self,
        space_id: &str,
        created_by_agent_id: Option<&str>,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<Node>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::ListMemories {
            space_id: space_id.to_owned(),
            created_by_agent_id: created_by_agent_id.map(str::to_owned),
            as_of,
            limit,
            reply,
        })?;
        receive(result)
    }

    fn forget_memory(
        &self,
        space_id: &str,
        node_id: &str,
        agent_id: &str,
        session_id: &str,
        valid_to: Timestamp,
        recorded_at: Timestamp,
    ) -> StorageResult<()> {
        self.write(WriteOperation::ForgetMemory {
            space_id: space_id.to_owned(),
            node_id: node_id.to_owned(),
            agent_id: agent_id.to_owned(),
            session_id: session_id.to_owned(),
            valid_to,
            recorded_at,
        })
    }

    fn search_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::SearchNodes {
            space_id: space_id.to_owned(),
            query: query.to_owned(),
            as_of,
            limit,
            reply,
        })?;
        receive(result)
    }

    fn search_nodes_in_layers(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<SearchHit>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::SearchNodesInLayers {
            space_id: space_id.to_owned(),
            query: query.to_owned(),
            as_of,
            layers,
            limit,
            reply,
        })?;
        receive(result)
    }

    fn vector_search(
        &self,
        space_id: &str,
        model_id: &str,
        query_vector: &[f32],
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<VectorSearchHit>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::VectorSearch {
            space_id: space_id.to_owned(),
            model_id: model_id.to_owned(),
            query_vector: query_vector.to_vec(),
            as_of,
            layers,
            limit,
            reply,
        })?;
        receive(result)
    }

    fn exact_match_nodes(
        &self,
        space_id: &str,
        query: &str,
        as_of: AsOf,
        layers: SearchLayers,
        limit: usize,
    ) -> StorageResult<Vec<Node>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::ExactMatchNodes {
            space_id: space_id.to_owned(),
            query: query.to_owned(),
            as_of,
            layers,
            limit,
            reply,
        })?;
        receive(result)
    }

    fn insert_edge(&self, edge: &NewEdge) -> StorageResult<()> {
        self.write(WriteOperation::InsertEdges {
            edges: vec![edge.clone()],
            bulk: false,
        })
    }

    fn insert_edges(&self, edges: &[NewEdge]) -> StorageResult<()> {
        self.write(WriteOperation::InsertEdges {
            edges: edges.to_vec(),
            bulk: true,
        })
    }

    fn edge_as_of(
        &self,
        space_id: &str,
        edge_id: &str,
        as_of: AsOf,
    ) -> StorageResult<Option<Edge>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::EdgeAsOf {
            space_id: space_id.to_owned(),
            edge_id: edge_id.to_owned(),
            as_of,
            reply,
        })?;
        receive(result)
    }

    fn upsert_embedding(&self, embedding: &NewEmbedding) -> StorageResult<()> {
        self.write(WriteOperation::UpsertEmbeddings(vec![embedding.clone()]))
    }

    fn upsert_embeddings(&self, embeddings: &[NewEmbedding]) -> StorageResult<()> {
        self.write(WriteOperation::UpsertEmbeddings(embeddings.to_vec()))
    }

    fn record_node_access(
        &self,
        space_id: &str,
        node_id: &str,
        now: Timestamp,
    ) -> StorageResult<()> {
        self.write(WriteOperation::RecordNodeAccess {
            space_id: space_id.to_owned(),
            node_id: node_id.to_owned(),
            now,
        })
    }

    fn record_node_accesses(
        &self,
        space_id: &str,
        node_ids: &[String],
        now: Timestamp,
    ) -> StorageResult<()> {
        self.write(WriteOperation::RecordNodeAccesses {
            space_id: space_id.to_owned(),
            node_ids: node_ids.to_vec(),
            now,
        })
    }

    fn materialized_paths_from(
        &self,
        space_id: &str,
        node_id: &str,
        limit: usize,
    ) -> StorageResult<Vec<MaterializedPath>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::MaterializedPaths {
            space_id: space_id.to_owned(),
            node_id: node_id.to_owned(),
            limit,
            reply,
        })?;
        receive(result)
    }

    fn materialized_path_count(&self, space_id: &str) -> StorageResult<usize> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::MaterializedPathCount {
            space_id: space_id.to_owned(),
            reply,
        })?;
        receive(result)
    }

    fn graph_neighbors(
        &self,
        space_id: &str,
        source_node_ids: &[String],
        as_of: AsOf,
        layers: SearchLayers,
        memory_only: bool,
        limit_per_source: usize,
    ) -> StorageResult<Vec<GraphNeighbor>> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::GraphNeighbors {
            space_id: space_id.to_owned(),
            source_node_ids: source_node_ids.to_vec(),
            as_of,
            layers,
            memory_only,
            limit_per_source,
            reply,
        })?;
        receive(result)
    }

    fn has_edges(&self, space_id: &str) -> StorageResult<bool> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::HasEdges {
            space_id: space_id.to_owned(),
            reply,
        })?;
        receive(result)
    }

    fn rebuild_paths(&self, space_id: &str) -> StorageResult<usize> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::RebuildPaths {
            space_id: space_id.to_owned(),
            reply,
        })?;
        receive(result)
    }

    fn maintenance(
        &self,
        now: Timestamp,
        batch_size: usize,
        allow_vacuum: bool,
    ) -> StorageResult<MaintenanceReport> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Maintenance {
            now,
            batch_size,
            allow_vacuum,
            reply,
        })?;
        receive(result)
    }

    fn node_count(&self, space_id: &str) -> StorageResult<usize> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::NodeCount {
            space_id: space_id.to_owned(),
            reply,
        })?;
        receive(result)
    }

    fn status(&self, space_id: &str, as_of: AsOf) -> StorageResult<StorageStatus> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Status {
            space_id: space_id.to_owned(),
            as_of,
            reply,
        })?;
        receive(result)
    }

    fn flush(&self) -> StorageResult<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.send(Command::Flush(reply))?;
        receive(result)
    }
}

fn receive<T>(receiver: mpsc::Receiver<StorageResult<T>>) -> StorageResult<T> {
    receiver
        .recv()
        .map_err(|error| StorageError::ActorUnavailable(error.to_string()))?
}

fn actor_loop(mut database: SqliteDatabase, receiver: mpsc::Receiver<Command>) {
    let mut deferred = VecDeque::new();
    let mut last_background_tick = Instant::now();

    loop {
        let command = deferred
            .pop_front()
            .or_else(|| receiver.recv_timeout(Duration::from_secs(1)).ok());
        let Some(command) = command else {
            maybe_background_tick(&mut database, &mut last_background_tick);
            continue;
        };

        match command {
            Command::Write(operation, reply) => {
                let mut writes = vec![(operation, reply)];
                let deadline = Instant::now() + WRITE_BATCH_INTERVAL;
                while writes.len() < MAX_WRITE_BATCH {
                    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                        break;
                    };
                    match receiver.recv_timeout(remaining) {
                        Ok(Command::Write(operation, reply)) => writes.push((operation, reply)),
                        Ok(command) => {
                            deferred.push_back(command);
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }

                let operations = writes
                    .iter_mut()
                    .map(|(operation, _)| std::mem::replace(operation, WriteOperation::Noop))
                    .collect();
                let results = database.apply_write_batch(operations);
                for ((_, reply), result) in writes.into_iter().zip(results) {
                    let _ = reply.send(result);
                }
            }
            Command::SchemaVersion(reply) => {
                let _ = reply.send(database.schema_version());
            }
            Command::NodeAsOf {
                space_id,
                node_id,
                as_of,
                reply,
            } => {
                let _ = reply.send(database.node_as_of(&space_id, &node_id, as_of));
            }
            Command::ListMemories {
                space_id,
                created_by_agent_id,
                as_of,
                limit,
                reply,
            } => {
                let _ = reply.send(database.list_memories(
                    &space_id,
                    created_by_agent_id.as_deref(),
                    as_of,
                    limit,
                ));
            }
            Command::SearchNodes {
                space_id,
                query,
                as_of,
                limit,
                reply,
            } => {
                let _ = reply.send(database.search_nodes(&space_id, &query, as_of, limit));
            }
            Command::SearchNodesInLayers {
                space_id,
                query,
                as_of,
                layers,
                limit,
                reply,
            } => {
                let _ = reply
                    .send(database.search_nodes_in_layers(&space_id, &query, as_of, layers, limit));
            }
            Command::VectorSearch {
                space_id,
                model_id,
                query_vector,
                as_of,
                layers,
                limit,
                reply,
            } => {
                let _ = reply.send(database.vector_search(
                    &space_id,
                    &model_id,
                    &query_vector,
                    as_of,
                    layers,
                    limit,
                ));
            }
            Command::ExactMatchNodes {
                space_id,
                query,
                as_of,
                layers,
                limit,
                reply,
            } => {
                let _ =
                    reply.send(database.exact_match_nodes(&space_id, &query, as_of, layers, limit));
            }
            Command::EdgeAsOf {
                space_id,
                edge_id,
                as_of,
                reply,
            } => {
                let _ = reply.send(database.edge_as_of(&space_id, &edge_id, as_of));
            }
            Command::MaterializedPaths {
                space_id,
                node_id,
                limit,
                reply,
            } => {
                let _ = reply.send(database.materialized_paths_from(&space_id, &node_id, limit));
            }
            Command::MaterializedPathCount { space_id, reply } => {
                let _ = reply.send(database.materialized_path_count(&space_id));
            }
            Command::HasEdges { space_id, reply } => {
                let _ = reply.send(database.has_edges(&space_id));
            }
            Command::RebuildPaths { space_id, reply } => {
                let _ = reply.send(database.rebuild_paths(&space_id));
            }
            Command::GraphNeighbors {
                space_id,
                source_node_ids,
                as_of,
                layers,
                memory_only,
                limit_per_source,
                reply,
            } => {
                let _ = reply.send(database.graph_neighbors(
                    &space_id,
                    &source_node_ids,
                    as_of,
                    layers,
                    memory_only,
                    limit_per_source,
                ));
            }
            Command::Maintenance {
                now,
                batch_size,
                allow_vacuum,
                reply,
            } => {
                let _ = reply.send(database.maintenance(now, batch_size, allow_vacuum));
            }
            Command::NodeCount { space_id, reply } => {
                let _ = reply.send(database.node_count(&space_id));
            }
            Command::Status {
                space_id,
                as_of,
                reply,
            } => {
                let _ = reply.send(database.status(&space_id, as_of));
            }
            Command::Flush(reply) => {
                let _ = reply.send(database.checkpoint());
            }
            Command::Shutdown => break,
        }

        maybe_background_tick(&mut database, &mut last_background_tick);
    }
}

fn maybe_background_tick(database: &mut SqliteDatabase, last_tick: &mut Instant) {
    if last_tick.elapsed() < BACKGROUND_TICK_INTERVAL {
        return;
    }
    let _ = database.maintenance(now_unix_seconds(), BACKGROUND_BATCH_SIZE, true);
    *last_tick = Instant::now();
}

fn now_unix_seconds() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}
