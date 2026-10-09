# A3/A6 synthetic benchmark

The benchmark is deterministic and uses a fresh SQLite database, 100,000 code
nodes, 300,000 directed edges, 1,000 relevant 256-dimensional vectors, and the
resulting 900,000 second-order paths. The write timer includes nodes, vectors,
edges, path materialization, commit, and WAL checkpoint. Search uses all three
signals and a 600-token budget. Latency statistics use 20 search iterations and
100 path iterations after one warm-up.

Run:

```console
cargo build --release -p brain-core --example synthetic_benchmark
BRAIN_BENCH_DB=target/a3a6-benchmark-100k.sqlite \
  BRAIN_BENCH_NODES=100000 \
  BRAIN_BENCH_EDGES_PER_NODE=3 \
  BRAIN_BENCH_SEARCH_ITERS=20 \
  BRAIN_BENCH_PATH_ITERS=100 \
  target/release/examples/synthetic_benchmark
```

Recorded result (2026-08-18):

- CPU: Intel Core i5-8250U, 4 cores / 8 threads
- OS: Linux 7.0.0-29-generic, x86_64
- Rust: 1.96.0

```text
nodes=100000
edges=300000
embeddings=1000
materialized_paths=900000
search_hits=15
full_write_ms=25084
nodes_write_ms=10951
embeddings_write_ms=117
edges_and_paths_write_ms=13942
bm25_probe_us=98683
vector_probe_us=11658
exact_probe_us=46271
search_median_us=160206
search_p95_us=173940
path2_median_us=69
path2_p95_us=109
```

The second-order path target is under 50 ms. Observed p95 was 0.109 ms.

An earlier full run was stopped after 15 minutes because bundled SQLite chose a
poor join order across FTS5/vec0 virtual tables and relational rows. Candidate
row IDs are now materialized first and bi-temporal nodes are loaded in a second
phase; the failed run is intentionally noted rather than omitted.
