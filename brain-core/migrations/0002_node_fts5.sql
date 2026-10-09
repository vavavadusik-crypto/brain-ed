-- External-content FTS keeps searchable text in one place while retaining every
-- historical assertion. Bi-temporal filtering happens after the FTS match.
CREATE VIRTUAL TABLE node_versions_fts USING fts5(
    label,
    body,
    source,
    content = 'node_versions',
    content_rowid = 'row_id',
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TRIGGER node_versions_fts_insert
AFTER INSERT ON node_versions
BEGIN
    INSERT INTO node_versions_fts(rowid, label, body, source)
    VALUES (new.row_id, new.label, new.body, new.source);
END;

CREATE TRIGGER node_versions_fts_delete
AFTER DELETE ON node_versions
BEGIN
    INSERT INTO node_versions_fts(node_versions_fts, rowid, label, body, source)
    VALUES ('delete', old.row_id, old.label, old.body, old.source);
END;

CREATE TRIGGER node_versions_fts_update
AFTER UPDATE OF label, body, source ON node_versions
BEGIN
    INSERT INTO node_versions_fts(node_versions_fts, rowid, label, body, source)
    VALUES ('delete', old.row_id, old.label, old.body, old.source);
    INSERT INTO node_versions_fts(rowid, label, body, source)
    VALUES (new.row_id, new.label, new.body, new.source);
END;

INSERT INTO node_versions_fts(rowid, label, body, source)
SELECT row_id, label, body, source FROM node_versions;

