-- A3/A6 storage primitives. Existing assertions remain untouched; derived
-- indexes are rebuilt from their canonical relational rows.

ALTER TABLE nodes ADD COLUMN source_weight REAL NOT NULL DEFAULT 1.0
    CHECK (source_weight BETWEEN 0.0 AND 1.0);
ALTER TABLE nodes ADD COLUMN degree INTEGER NOT NULL DEFAULT 0
    CHECK (degree >= 0);
ALTER TABLE nodes ADD COLUMN salience_layer TEXT NOT NULL DEFAULT 'hot'
    CHECK (salience_layer IN ('hot', 'warm', 'cold'));
ALTER TABLE nodes ADD COLUMN token_len INTEGER NOT NULL DEFAULT 1
    CHECK (token_len > 0);
ALTER TABLE nodes ADD COLUMN next_salience_at INTEGER NOT NULL DEFAULT 0;

UPDATE nodes
SET degree = (
        SELECT count(*)
        FROM edges
        WHERE edges.space_id = nodes.space_id
          AND (edges.from_node_id = nodes.id OR edges.to_node_id = nodes.id)
    ),
    salience_layer = CASE
        WHEN salience >= 0.70 THEN 'hot'
        WHEN salience >= 0.35 THEN 'warm'
        ELSE 'cold'
    END,
    token_len = coalesce((
        SELECT max(1, (length(v.label) + length(v.body) + 3) / 4)
        FROM node_versions AS v
        WHERE v.space_id = nodes.space_id
          AND v.node_id = nodes.id
          AND v.superseded_at IS NULL
        ORDER BY v.recorded_at DESC, v.revision DESC
        LIMIT 1
    ), 1);

CREATE INDEX idx_nodes_space_layer_next
    ON nodes(space_id, salience_layer, next_salience_at, salience DESC);
CREATE INDEX idx_nodes_agent_created
    ON nodes(created_by_agent_id, created_at, space_id);
CREATE INDEX idx_node_versions_space_kind_current
    ON node_versions(space_id, kind, superseded_at, node_id);

-- Replace the v2 FTS index because product search is deliberately limited to
-- label and body. Source paths remain ordinary relational metadata.
DROP TRIGGER node_versions_fts_insert;
DROP TRIGGER node_versions_fts_delete;
DROP TRIGGER node_versions_fts_update;
DROP TABLE node_versions_fts;

CREATE VIRTUAL TABLE node_versions_fts USING fts5(
    label,
    body,
    content = 'node_versions',
    content_rowid = 'row_id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER node_versions_fts_insert
AFTER INSERT ON node_versions
BEGIN
    INSERT INTO node_versions_fts(rowid, label, body)
    VALUES (new.row_id, new.label, new.body);
END;

CREATE TRIGGER node_versions_fts_delete
AFTER DELETE ON node_versions
BEGIN
    INSERT INTO node_versions_fts(node_versions_fts, rowid, label, body)
    VALUES ('delete', old.row_id, old.label, old.body);
END;

CREATE TRIGGER node_versions_fts_update
AFTER UPDATE OF label, body ON node_versions
BEGIN
    INSERT INTO node_versions_fts(node_versions_fts, rowid, label, body)
    VALUES ('delete', old.row_id, old.label, old.body);
    INSERT INTO node_versions_fts(rowid, label, body)
    VALUES (new.row_id, new.label, new.body);
END;

INSERT INTO node_versions_fts(rowid, label, body)
SELECT row_id, label, body FROM node_versions;

-- Version 3 fixed vectors at 256 dimensions. Keep those tables for backward
-- compatibility and copy their data into the partitioned index used now.
CREATE TABLE embedding_records (
    id              INTEGER PRIMARY KEY,
    node_version_id INTEGER NOT NULL,
    space_id        TEXT NOT NULL REFERENCES spaces(id),
    model_id        TEXT NOT NULL,
    dimensions      INTEGER NOT NULL CHECK (dimensions IN (256, 384)),
    embedded_at     INTEGER NOT NULL,
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    UNIQUE (node_version_id, model_id),
    FOREIGN KEY (node_version_id, space_id)
        REFERENCES node_versions(row_id, space_id),
    FOREIGN KEY (session_id, agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

CREATE INDEX idx_embedding_records_space_model
    ON embedding_records(space_id, model_id, dimensions, node_version_id);
CREATE INDEX idx_embedding_records_agent_time
    ON embedding_records(agent_id, embedded_at);

CREATE VIRTUAL TABLE node_embeddings_vec_256 USING vec0(
    embedding_id INTEGER PRIMARY KEY,
    space_id TEXT PARTITION KEY,
    model_id TEXT PARTITION KEY,
    embedding FLOAT[256] distance_metric=cosine
);

CREATE VIRTUAL TABLE node_embeddings_vec_384 USING vec0(
    embedding_id INTEGER PRIMARY KEY,
    space_id TEXT PARTITION KEY,
    model_id TEXT PARTITION KEY,
    embedding FLOAT[384] distance_metric=cosine
);

INSERT OR IGNORE INTO embedding_records(
    id, node_version_id, space_id, model_id, dimensions, embedded_at,
    agent_id, session_id
)
SELECT row_id, node_version_id, space_id, model_id, dimensions, embedded_at,
       agent_id, session_id
FROM node_embeddings;

INSERT OR IGNORE INTO node_embeddings_vec_256(
    embedding_id, space_id, model_id, embedding
)
SELECT old.embedding_id, records.space_id, records.model_id, old.embedding
FROM node_embeddings_vec AS old
JOIN embedding_records AS records ON records.id = old.embedding_id;

CREATE TABLE materialized_paths (
    space_id       TEXT NOT NULL REFERENCES spaces(id),
    a              TEXT NOT NULL,
    b              TEXT NOT NULL,
    c              TEXT NOT NULL,
    ab_edge_id     TEXT NOT NULL,
    bc_edge_id     TEXT NOT NULL,
    path_weight    REAL NOT NULL CHECK (path_weight BETWEEN 0.0 AND 1.0),
    path_kind      TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (space_id, a, b, c)
) STRICT;

CREATE INDEX idx_paths_from
    ON materialized_paths(space_id, a, path_weight DESC, c);
CREATE INDEX idx_paths_to
    ON materialized_paths(space_id, c, path_weight DESC, a);
CREATE INDEX idx_paths_hub
    ON materialized_paths(space_id, b);
CREATE INDEX idx_paths_kind
    ON materialized_paths(space_id, path_kind, a, c);

CREATE TABLE path_update_queue (
    space_id   TEXT NOT NULL,
    edge_id    TEXT NOT NULL,
    queued_at  INTEGER NOT NULL,
    PRIMARY KEY (space_id, edge_id),
    FOREIGN KEY (space_id, edge_id) REFERENCES edges(space_id, id)
) STRICT;

CREATE INDEX idx_path_update_queue_time
    ON path_update_queue(queued_at, space_id, edge_id);

CREATE TABLE maintenance_state (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    last_tick_at    INTEGER NOT NULL DEFAULT 0,
    last_vacuum_at  INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT INTO maintenance_state(singleton, last_tick_at, last_vacuum_at)
VALUES (1, unixepoch(), unixepoch());
