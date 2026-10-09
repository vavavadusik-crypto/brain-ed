-- The default multilingual Model2Vec model emits 256-dimensional vectors.
-- Metadata remains relational; vec0 stores the compact vector payload and uses
-- the same integer key. New dimensions can be added by a later migration.
CREATE TABLE node_embeddings (
    row_id          INTEGER PRIMARY KEY,
    node_version_id INTEGER NOT NULL,
    space_id        TEXT NOT NULL REFERENCES spaces(id),
    model_id        TEXT NOT NULL,
    dimensions      INTEGER NOT NULL DEFAULT 256 CHECK (dimensions = 256),
    embedded_at     INTEGER NOT NULL,
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    UNIQUE (node_version_id, model_id),
    FOREIGN KEY (node_version_id, space_id)
        REFERENCES node_versions(row_id, space_id),
    FOREIGN KEY (session_id, agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

CREATE INDEX idx_node_embeddings_space_model
    ON node_embeddings(space_id, model_id, node_version_id);
CREATE INDEX idx_node_embeddings_agent_time
    ON node_embeddings(agent_id, embedded_at);

CREATE VIRTUAL TABLE node_embeddings_vec USING vec0(
    embedding_id INTEGER PRIMARY KEY,
    embedding FLOAT[256] distance_metric=cosine
);
