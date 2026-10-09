-- Stable identities are separated from bi-temporal assertions. This lets the
-- engine preserve both what was believed and when that belief applied.

CREATE TABLE agents (
    id            TEXT PRIMARY KEY,
    display_name  TEXT,
    metadata      TEXT NOT NULL DEFAULT '{}'
                  CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE spaces (
    id                    TEXT PRIMARY KEY,
    kind                  TEXT NOT NULL
                          CHECK (kind IN ('project', 'repository', 'personal', 'custom')),
    label                 TEXT NOT NULL,
    metadata              TEXT NOT NULL DEFAULT '{}'
                          CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    created_by_agent_id   TEXT NOT NULL REFERENCES agents(id),
    created_at            INTEGER NOT NULL
) STRICT;

CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,
    space_id    TEXT NOT NULL REFERENCES spaces(id),
    agent_id    TEXT NOT NULL REFERENCES agents(id),
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER CHECK (ended_at IS NULL OR ended_at >= started_at),
    metadata    TEXT NOT NULL DEFAULT '{}'
                CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    UNIQUE (id, agent_id, space_id)
) STRICT;

CREATE TABLE nodes (
    id                      TEXT NOT NULL,
    space_id                TEXT NOT NULL REFERENCES spaces(id),
    created_by_agent_id     TEXT NOT NULL,
    created_in_session_id   TEXT NOT NULL,
    created_at              INTEGER NOT NULL,
    last_accessed           INTEGER NOT NULL,
    access_count            INTEGER NOT NULL DEFAULT 0 CHECK (access_count >= 0),
    salience                REAL NOT NULL DEFAULT 1.0 CHECK (salience BETWEEN 0.0 AND 1.0),
    PRIMARY KEY (space_id, id),
    FOREIGN KEY (created_in_session_id, created_by_agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

CREATE TABLE node_versions (
    row_id          INTEGER PRIMARY KEY,
    node_id         TEXT NOT NULL,
    space_id        TEXT NOT NULL,
    revision        INTEGER NOT NULL CHECK (revision > 0),
    kind            TEXT NOT NULL
                    CHECK (kind IN ('code', 'document', 'memory', 'session',
                                    'decision', 'error', 'concept', 'entity')),
    label           TEXT NOT NULL,
    body            TEXT NOT NULL,
    source          TEXT,
    metadata        TEXT NOT NULL DEFAULT '{}'
                    CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    valid_from      INTEGER NOT NULL,
    valid_to        INTEGER CHECK (valid_to IS NULL OR valid_to > valid_from),
    recorded_at     INTEGER NOT NULL,
    superseded_at   INTEGER CHECK (superseded_at IS NULL OR superseded_at > recorded_at),
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    UNIQUE (row_id, space_id),
    UNIQUE (space_id, node_id, revision),
    FOREIGN KEY (space_id, node_id) REFERENCES nodes(space_id, id),
    FOREIGN KEY (session_id, agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

CREATE TABLE edges (
    id                      TEXT NOT NULL,
    space_id                TEXT NOT NULL REFERENCES spaces(id),
    from_node_id            TEXT NOT NULL,
    to_node_id              TEXT NOT NULL,
    created_by_agent_id     TEXT NOT NULL,
    created_in_session_id   TEXT NOT NULL,
    created_at              INTEGER NOT NULL,
    last_accessed           INTEGER NOT NULL,
    access_count            INTEGER NOT NULL DEFAULT 0 CHECK (access_count >= 0),
    salience                REAL NOT NULL DEFAULT 1.0 CHECK (salience BETWEEN 0.0 AND 1.0),
    PRIMARY KEY (space_id, id),
    FOREIGN KEY (space_id, from_node_id) REFERENCES nodes(space_id, id),
    FOREIGN KEY (space_id, to_node_id) REFERENCES nodes(space_id, id),
    FOREIGN KEY (created_in_session_id, created_by_agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

CREATE TABLE edge_versions (
    row_id          INTEGER PRIMARY KEY,
    edge_id         TEXT NOT NULL,
    space_id        TEXT NOT NULL,
    revision        INTEGER NOT NULL CHECK (revision > 0),
    relation        TEXT NOT NULL CHECK (length(relation) > 0),
    weight          REAL NOT NULL CHECK (weight BETWEEN 0.0 AND 1.0),
    confidence      REAL NOT NULL CHECK (confidence BETWEEN 0.0 AND 1.0),
    metadata        TEXT NOT NULL DEFAULT '{}'
                    CHECK (json_valid(metadata) AND json_type(metadata) = 'object'),
    valid_from      INTEGER NOT NULL,
    valid_to        INTEGER CHECK (valid_to IS NULL OR valid_to > valid_from),
    recorded_at     INTEGER NOT NULL,
    superseded_at   INTEGER CHECK (superseded_at IS NULL OR superseded_at > recorded_at),
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    UNIQUE (space_id, edge_id, revision),
    FOREIGN KEY (space_id, edge_id) REFERENCES edges(space_id, id),
    FOREIGN KEY (session_id, agent_id, space_id)
        REFERENCES sessions(id, agent_id, space_id)
) STRICT;

-- Isolation and attribution indexes.
CREATE INDEX idx_sessions_space_agent_time
    ON sessions(space_id, agent_id, started_at);
CREATE INDEX idx_nodes_space
    ON nodes(space_id, id);
CREATE INDEX idx_node_versions_space_kind
    ON node_versions(space_id, kind, node_id);
CREATE INDEX idx_node_versions_agent_time
    ON node_versions(agent_id, recorded_at);
CREATE INDEX idx_edges_space_endpoints
    ON edges(space_id, from_node_id, to_node_id);
CREATE INDEX idx_edge_versions_space_relation
    ON edge_versions(space_id, relation, edge_id);
CREATE INDEX idx_edge_versions_agent_time
    ON edge_versions(agent_id, recorded_at);

-- Bi-temporal indexes. All interval ends are exclusive and NULL means infinity.
CREATE INDEX idx_node_versions_as_of
    ON node_versions(node_id, space_id, valid_from, valid_to,
                     recorded_at, superseded_at);
CREATE INDEX idx_node_versions_valid_time
    ON node_versions(space_id, valid_from, valid_to);
CREATE INDEX idx_node_versions_recorded_time
    ON node_versions(space_id, recorded_at, superseded_at);
CREATE INDEX idx_edge_versions_as_of
    ON edge_versions(edge_id, space_id, valid_from, valid_to,
                     recorded_at, superseded_at);
CREATE INDEX idx_edge_versions_valid_time
    ON edge_versions(space_id, valid_from, valid_to);
CREATE INDEX idx_edge_versions_recorded_time
    ON edge_versions(space_id, recorded_at, superseded_at);

-- Decay scans operate on logical records, not duplicate historical assertions.
CREATE INDEX idx_nodes_decay
    ON nodes(space_id, salience, last_accessed, access_count);
CREATE INDEX idx_edges_decay
    ON edges(space_id, salience, last_accessed, access_count);
