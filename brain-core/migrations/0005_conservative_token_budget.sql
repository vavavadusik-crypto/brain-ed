-- Existing v4 rows estimated tokens as Unicode scalar count / 4, which can
-- undercount Cyrillic and other multilingual text. A UTF-8 byte count is a
-- conservative upper bound for BPE tokenizers and therefore preserves the
-- hard search-budget guarantee without adding a tokenizer to the hot path.

UPDATE nodes
SET token_len = coalesce((
    SELECT max(
        1,
        length(CAST(v.label AS BLOB))
            + length(CAST(v.body AS BLOB))
            + 1
    )
    FROM node_versions AS v
    WHERE v.space_id = nodes.space_id
      AND v.node_id = nodes.id
      AND v.superseded_at IS NULL
    ORDER BY v.recorded_at DESC, v.revision DESC
    LIMIT 1
), 1);
