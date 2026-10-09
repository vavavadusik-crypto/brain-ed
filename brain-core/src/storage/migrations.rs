//! Ordered, append-only database migrations.

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 5;

pub(super) struct Migration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub(super) const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "core_bitemporal_graph",
        sql: include_str!("../../migrations/0001_core_bitemporal_graph.sql"),
    },
    Migration {
        version: 2,
        name: "node_fts5",
        sql: include_str!("../../migrations/0002_node_fts5.sql"),
    },
    Migration {
        version: 3,
        name: "node_vectors",
        sql: include_str!("../../migrations/0003_node_vectors.sql"),
    },
    Migration {
        version: 4,
        name: "salience_search_paths",
        sql: include_str!("../../migrations/0004_salience_search_paths.sql"),
    },
    Migration {
        version: 5,
        name: "conservative_token_budget",
        sql: include_str!("../../migrations/0005_conservative_token_budget.sql"),
    },
];
