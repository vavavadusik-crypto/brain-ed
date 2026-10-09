# Storage schema

Schema version 5 keeps the version-4 salience/vector/path layout and migrates
persisted token lengths to a conservative UTF-8 byte upper bound. This prevents
multilingual text from exceeding a requested search budget. Timestamps are
signed Unix seconds. Every interval is half-open: the start is inclusive, the
end is exclusive, and a `NULL` end means infinity.

## Identity and isolation

| Record | Purpose |
| --- | --- |
| `agents` | Stable author identities such as an editor agent, coding agent, or human. |
| `spaces` | Project, repository, personal, or custom isolation boundaries. |
| `sessions` | One agent working in exactly one space. |
| `nodes` | Stable identity plus source weight, degree, access state, salience layer, token estimate, and next maintenance deadline. |
| `node_versions` | Versioned kind, label, body, source, metadata, attribution, and both time intervals. |
| `edges` | Stable directed endpoints plus access/decay state. Both endpoints must exist in the same space. |
| `edge_versions` | Versioned relation, trust weight, confidence, metadata, attribution, and both time intervals. |

Node and edge identifiers are scoped by `space_id`; the same caller-provided ID may exist in two spaces without mixing their histories. Composite foreign keys also require each version's `(session_id, agent_id, space_id)` to match a real session.

`weight` expresses the strength of the evidence type, while `confidence` expresses certainty about this particular extraction. Callers can therefore rank deterministic AST `calls` above path-derived `mentions`, and those above lexical `relates_to`, without conflating that policy with extraction confidence. Both values are constrained to the inclusive range 0 through 1.

## Memory kinds

The product-level memory enum is identical in the CLI and MCP schemas:
`fact | decision | task | gotcha | error | note`. It is persisted in node
metadata. `decision` maps to the database node kind `decision`; `gotcha` and
`error` map to `error`; `fact`, `task`, and `note` map to `memory`. This keeps
the graph-wide node enum (`code`, `document`, `memory`, `session`, `decision`,
`error`, `concept`, `entity`) separate from the user-facing memory taxonomy
without losing the first-class `error` type.

## Bi-temporal history

Each version has two independent intervals:

- `valid_from` / `valid_to`: when the assertion is true in the outside world.
- `recorded_at` / `superseded_at`: when the database believes that assertion.

Replacing a fact at world time `V2`, learned at system time `R2`, is one transaction:

1. Set `superseded_at = R2` on the current assertion that contains `V2`.
2. Insert a current assertion with the old content and validity `[old.valid_from, V2)`.
3. Insert a current assertion with the new content and validity `[V2, old.valid_to)`.

The original row remains available in `[old.recorded_at, R2)`. This preserves both questions: “what was true at `V` given everything known now?” and “what did the system believe at `R`?” No historical row is deleted.

## One-query as-of lookup

Both temporal coordinates are resolved by one parameterized SQL statement:

```sql
SELECT n.id, v.*
FROM node_versions AS v
JOIN nodes AS n ON n.id = v.node_id AND n.space_id = v.space_id
WHERE v.space_id = :space_id
  AND v.node_id = :node_id
  AND v.valid_from <= :valid_at
  AND (v.valid_to IS NULL OR :valid_at < v.valid_to)
  AND v.recorded_at <= :recorded_at
  AND (v.superseded_at IS NULL OR :recorded_at < v.superseded_at)
ORDER BY v.recorded_at DESC, v.revision DESC
LIMIT 1;
```

`idx_node_versions_as_of` and `idx_edge_versions_as_of` cover the identity and interval predicates. Separate indexes support valid-time scans, recorded-time scans, space/kind or space/relation queries, agent attribution, session history, and decay candidates.

## Text and vectors

`node_versions_fts` is an FTS5 external-content table over `label` and `body`.
Triggers keep every historical assertion searchable; the storage query
materializes a bounded FTS result before applying both time windows,
`space_id`, and salience-layer filters.

`embedding_records` stores attribution and model metadata.
`node_embeddings_vec_256` and `node_embeddings_vec_384` are statically linked
`sqlite-vec` `vec0` tables. Both partition by `space_id` and `model_id`, use
cosine distance, and are keyed by `embedding_records.id`. Version-3 vectors are
copied forward during migration; the old table remains readable for backward
compatibility.

New records persist `label bytes + body bytes + 1` as their conservative token
cost. Since a BPE token consumes at least one UTF-8 byte, this may under-fill a
budget but cannot overrun it because of multilingual text.

## Salience and maintenance

`nodes.salience_layer` is `hot`, `warm`, or `cold`; ordinary search filters to
hot without deleting canonical content. `next_salience_at` makes background
decay incremental rather than an O(N) scan. Accesses immediately update the
counter, timestamp, formula score, layer, and next deadline.

The writer actor runs bounded background ticks for due salience rows and
deferred high-degree path updates. `maintenance_state` persists tick/VACUUM
timestamps; a new database does not immediately vacuum. `flush()` commits prior
actor commands and truncates the WAL through a checkpoint.

## Materialized second-order paths

`materialized_paths` stores `(space_id, a, b, c)`, both edge IDs, the product of
edge weights, concatenated relation kind, and timestamps. Indexes support
top-weight queries from/to a node, intermediate hubs, and path kind. Ordinary
edge insertion updates the two affected path families. Large imports project
current edge versions into a temporary indexed table, rebuild paths set-wise,
then recreate secondary indexes. High-degree incremental work is queued in
`path_update_queue` for maintenance.

Only the derived path table omits foreign keys: every row is generated from
canonical nodes/edges in the same transaction, and avoiding five redundant FK
lookups is material to bulk-build performance. Canonical graph tables retain
their composite isolation constraints.

## Migration rules

Migrations in `brain-core/migrations` are ordered and embedded in the binary.
Each migration runs in an immediate transaction and is recorded in
`schema_migrations`; `PRAGMA user_version` must agree with that ledger. A
database newer than the engine fails closed. Once released, a migration is
never edited—changes require a new numbered migration.
