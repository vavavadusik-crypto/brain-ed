//! Reproducible A3/A6 synthetic benchmark (100k nodes / 300k edges by default).

use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::embeddings::{Embedder, EmbeddingError};
use brain_core::{
    Agent, AsOf, Attribution, HybridSearch, NewEdge, NewEmbedding, NewNode, NodeKind, SearchLayers,
    SearchQuery, Session, Space, SpaceKind, SqliteStorage, Storage,
};

const AGENT_ID: &str = "benchmark-agent";
const SPACE_ID: &str = "benchmark-space";
const SESSION_ID: &str = "benchmark-session";
const MODEL_ID: &str = "benchmark-deterministic-256";
const NOW: i64 = 1_787_000_000;

struct BenchmarkEmbedder;

impl Embedder for BenchmarkEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dimensions(&self) -> usize {
        256
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut vector = vec![0.0; 256];
        if text.to_lowercase().contains("compiler") {
            vector[0] = 1.0;
        } else {
            vector[1] = 1.0;
        }
        Ok(vector)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let node_count = env_usize("BRAIN_BENCH_NODES", 100_000);
    let edges_per_node = env_usize("BRAIN_BENCH_EDGES_PER_NODE", 3);
    let edge_count = node_count.saturating_mul(edges_per_node);
    let embedding_count = node_count.saturating_add(99) / 100;
    let search_iterations = env_usize("BRAIN_BENCH_SEARCH_ITERS", 20);
    let path_iterations = env_usize("BRAIN_BENCH_PATH_ITERS", 100);
    let database_path = benchmark_path();
    let storage = SqliteStorage::open(&database_path)?;
    let attribution = Attribution {
        agent_id: AGENT_ID.to_owned(),
        session_id: SESSION_ID.to_owned(),
        recorded_at: NOW,
    };
    create_scope(&storage)?;

    let write_started = Instant::now();
    let nodes_started = Instant::now();
    for start in (0..node_count).step_by(10_000) {
        let end = (start + 10_000).min(node_count);
        let nodes = (start..end)
            .map(|index| benchmark_node(index, &attribution))
            .collect::<Vec<_>>();
        storage.insert_nodes(&nodes)?;
    }
    let nodes_elapsed = nodes_started.elapsed();

    let embeddings_started = Instant::now();
    let embedding_indices = (0..node_count).step_by(100).collect::<Vec<_>>();
    for indices in embedding_indices.chunks(5_000) {
        let embeddings = indices
            .iter()
            .copied()
            .map(|index| benchmark_embedding(index, node_count, &attribution))
            .collect::<Vec<_>>();
        storage.upsert_embeddings(&embeddings)?;
    }
    let embeddings_elapsed = embeddings_started.elapsed();

    let edges_started = Instant::now();
    let mut edges = Vec::with_capacity(edge_count);
    for from in 0..node_count {
        for hop in 1..=edges_per_node {
            edges.push(NewEdge {
                id: format!("edge-{from}-{hop}"),
                space_id: SPACE_ID.to_owned(),
                from_node_id: format!("node-{from}"),
                to_node_id: format!("node-{}", (from + hop) % node_count),
                relation: if hop == 1 { "calls" } else { "uses" }.to_owned(),
                weight: 1.0 - hop as f64 / 10.0,
                confidence: 1.0,
                metadata_json: "{}".to_owned(),
                valid_from: NOW,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 1.0,
            });
        }
    }
    storage.insert_edges(&edges)?;
    let edges_elapsed = edges_started.elapsed();
    storage.flush()?;
    let write_elapsed = write_started.elapsed();

    let embedder = BenchmarkEmbedder;
    let search = HybridSearch::new(&storage, &embedder);
    let query = SearchQuery::new(
        SPACE_ID,
        "compiler retrieval",
        AsOf::new(NOW + 10, NOW + 10),
    );
    let probe_started = Instant::now();
    let _ = storage.search_nodes_in_layers(
        SPACE_ID,
        "\"compiler\" OR \"retrieval\"",
        query.as_of,
        SearchLayers::HOT,
        200,
    )?;
    let bm25_probe = probe_started.elapsed();
    let probe_started = Instant::now();
    let _ = storage.vector_search(
        SPACE_ID,
        MODEL_ID,
        &embedder.embed(&query.text)?,
        query.as_of,
        SearchLayers::HOT,
        200,
    )?;
    let vector_probe = probe_started.elapsed();
    let probe_started = Instant::now();
    let _ =
        storage.exact_match_nodes(SPACE_ID, &query.text, query.as_of, SearchLayers::HOT, 200)?;
    let exact_probe = probe_started.elapsed();
    let warmup_search = search.search(&query, 600)?;
    let search_hits = warmup_search.hits.len();
    let search_timings = measure(search_iterations, || search.search(&query, 600).map(|_| ()))?;

    let _ = storage.materialized_paths_from(SPACE_ID, "node-0", 50)?;
    let path_timings = measure(path_iterations, || {
        storage
            .materialized_paths_from(SPACE_ID, "node-0", 50)
            .map(|_| ())
    })?;
    let materialized_paths = storage.materialized_path_count(SPACE_ID)?;

    println!("database={}", database_path.display());
    println!("nodes={node_count}");
    println!("edges={edge_count}");
    println!("embeddings={embedding_count}");
    println!("materialized_paths={materialized_paths}");
    println!("search_hits={search_hits}");
    println!("full_write_ms={}", write_elapsed.as_millis());
    println!("nodes_write_ms={}", nodes_elapsed.as_millis());
    println!("embeddings_write_ms={}", embeddings_elapsed.as_millis());
    println!("edges_and_paths_write_ms={}", edges_elapsed.as_millis());
    println!("bm25_probe_us={}", bm25_probe.as_micros());
    println!("vector_probe_us={}", vector_probe.as_micros());
    println!("exact_probe_us={}", exact_probe.as_micros());
    println!(
        "search_median_us={}",
        percentile(&search_timings, 50).as_micros()
    );
    println!(
        "search_p95_us={}",
        percentile(&search_timings, 95).as_micros()
    );
    println!(
        "path2_median_us={}",
        percentile(&path_timings, 50).as_micros()
    );
    println!("path2_p95_us={}", percentile(&path_timings, 95).as_micros());
    Ok(())
}

fn create_scope(storage: &SqliteStorage) -> Result<(), Box<dyn Error>> {
    storage.create_agent(&Agent {
        id: AGENT_ID.to_owned(),
        display_name: Some("Synthetic benchmark".to_owned()),
        metadata_json: "{}".to_owned(),
        created_at: NOW,
    })?;
    storage.create_space(&Space {
        id: SPACE_ID.to_owned(),
        kind: SpaceKind::Project,
        label: "Synthetic benchmark".to_owned(),
        metadata_json: "{}".to_owned(),
        created_by_agent_id: AGENT_ID.to_owned(),
        created_at: NOW,
    })?;
    storage.create_session(&Session {
        id: SESSION_ID.to_owned(),
        space_id: SPACE_ID.to_owned(),
        agent_id: AGENT_ID.to_owned(),
        started_at: NOW,
        ended_at: None,
        metadata_json: "{}".to_owned(),
    })?;
    Ok(())
}

fn benchmark_node(index: usize, attribution: &Attribution) -> NewNode {
    let compiler = index.is_multiple_of(100);
    NewNode {
        id: format!("node-{index}"),
        space_id: SPACE_ID.to_owned(),
        kind: NodeKind::Code,
        label: if compiler {
            format!("Compiler retrieval node {index}")
        } else {
            format!("Synthetic memory node {index}")
        },
        body: if compiler {
            "Compiler retrieval path and local build memory".to_owned()
        } else {
            "Synthetic graph content for storage benchmarking".to_owned()
        },
        source: None,
        metadata_json: "{}".to_owned(),
        valid_from: NOW,
        valid_to: None,
        attribution: attribution.clone(),
        salience: 1.0,
        source_weight: 1.0,
    }
}

fn benchmark_embedding(index: usize, node_count: usize, attribution: &Attribution) -> NewEmbedding {
    let mut vector = vec![0.0; 256];
    if index.is_multiple_of(100) {
        let rank = index as f32 / node_count.max(1) as f32;
        let primary = 0.9 - 0.2 * rank;
        vector[0] = primary;
        vector[index % 255 + 1] = (1.0 - primary * primary).sqrt();
    } else {
        vector[index % 255 + 1] = 1.0;
    }
    NewEmbedding {
        node_version_id: i64::try_from(index + 1).unwrap_or(i64::MAX),
        space_id: SPACE_ID.to_owned(),
        model_id: MODEL_ID.to_owned(),
        vector,
        embedded_at: NOW + 1,
        attribution: attribution.clone(),
    }
}

fn measure<E>(
    iterations: usize,
    mut operation: impl FnMut() -> Result<(), E>,
) -> Result<Vec<Duration>, E> {
    let mut timings = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        operation()?;
        timings.push(started.elapsed());
    }
    timings.sort_unstable();
    Ok(timings)
}

fn percentile(timings: &[Duration], percentile: usize) -> Duration {
    let index = timings.len().saturating_sub(1).saturating_mul(percentile) / 100;
    timings[index]
}

fn benchmark_path() -> PathBuf {
    if let Some(path) = std::env::var_os("BRAIN_BENCH_DB") {
        return PathBuf::from(path);
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "brain-for-ai-benchmark-{}-{nonce}.sqlite",
        std::process::id()
    ))
}

fn env_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}
