//! Rebuilds the vector index after a model change or an interrupted backfill.
//!
//! Vectors are written by the background worker as memories arrive, which
//! leaves two holes it cannot fill by itself: records stored before any model
//! was installed never get a job, and a switch to another model strands every
//! vector produced by the old one. Both states are silent — search simply
//! returns less — so this command exists to make them repairable.

use std::error::Error;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::time::{Duration, Instant};

#[cfg(feature = "model2vec")]
use brain_core::NewEmbedding;
use brain_core::embeddings::{DEFAULT_ENGLISH_MODEL2VEC_MODEL, DEFAULT_MODEL2VEC_MODEL};
#[cfg(feature = "model2vec")]
use brain_core::embeddings::{Embedder, Model2VecEmbedder, model_is_cached};
use brain_core::{Storage, Timestamp};
use clap::Args;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{RuntimeConfig, now_unix_seconds, print_json};
#[cfg(feature = "model2vec")]
use crate::{ensure_context, open_storage};

/// Vectors written between forced commits, matching the background worker.
///
/// The worker learned this the expensive way: committing per record turns a
/// quarter-second of encoding on ten thousand records into an hour of fsync.
#[cfg(feature = "model2vec")]
const FLUSH_EVERY: usize = 500;

/// Node versions fetched from SQLite per page.
#[cfg(feature = "model2vec")]
const PAGE_SIZE: usize = 500;

/// Distinct embedding failures reported before the rest are only counted.
#[cfg(any(feature = "model2vec", test))]
const MAX_REPORTED_ERRORS: usize = 3;

/// Wait applied to the read connection while the writer holds the database.
const READ_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Args)]
pub(crate) struct ReindexArgs {
    /// Rebuild under a different embedding model instead of the current one.
    #[arg(long, value_name = "ID")]
    model: Option<String>,

    /// Recompute vectors that already exist instead of only filling gaps.
    #[arg(long)]
    force: bool,

    /// Approve a rebuild that destroys stored vectors or search cannot read.
    #[arg(long)]
    yes: bool,

    /// Report the plan and exit without writing anything.
    #[arg(long)]
    dry_run: bool,

    /// Rebuild the two-hop graph paths instead of the vectors.
    ///
    /// Paths are maintained as edges arrive, so this is for a database whose
    /// edges came in by another route — an import from an older store, for
    /// instance, which leaves the graph walk with nothing to walk.
    #[arg(long, conflicts_with_all = ["model", "force", "yes"])]
    paths: bool,
}

/// State of one memory space before anything is rewritten.
struct Survey {
    /// Node versions live at the query point, so reachable by search.
    active_versions: usize,
    /// Live versions that already carry a vector under the target model.
    already_indexed: usize,
    /// Live versions with no vector under the target model.
    missing: usize,
    /// Live coverage held under other models, largest population first.
    other_models: Vec<(String, usize)>,
}

/// What this run destroys, and what it can never achieve.
///
/// Only these two outcomes deserve a prompt. Recomputing a stored vector throws
/// the old one away with no copy left, and writing vectors under a model search
/// cannot select spends the entire run producing rows nothing will ever read.
/// Everything else this command does is purely additive.
struct Hazard {
    /// Vectors recomputed in place, replacing what is stored today.
    overwritten: usize,
    /// Target model no query can select, which makes the whole run inert.
    unreachable: bool,
}

impl Hazard {
    const fn needs_approval(&self) -> bool {
        self.overwritten > 0 || self.unreachable
    }
}

/// Reports whether search can ever select this model for a query.
///
/// The query model is derived from the query text alone — `load_cached_embedder`
/// calls `model_id_for_text`, and the vector lookup then filters on that exact
/// `model_id`. No setting redirects a query elsewhere, so a vector stored under
/// any other model is unreadable by construction. Saying so up front is the
/// difference between a refused run and an hour of encoding reported as "ok".
fn reachable_by_search(model_id: &str) -> bool {
    model_id == DEFAULT_MODEL2VEC_MODEL || model_id == DEFAULT_ENGLISH_MODEL2VEC_MODEL
}

/// Names the routed model this space has no vectors for, if any.
///
/// Because routing is per query text, a space indexed under one model answers
/// only half its questions: with multilingual vectors alone every ASCII-only
/// query silently falls back to BM25, and nothing in the output ever says why.
/// The counts are already surveyed, so reporting the gap costs one comparison
/// and turns a half-outage into a line the user can act on.
fn coverage_gap(survey: &Survey, target: &str) -> Option<String> {
    if survey.active_versions == 0 {
        return None;
    }
    let (other, routed) = if target == DEFAULT_MODEL2VEC_MODEL {
        (
            DEFAULT_ENGLISH_MODEL2VEC_MODEL,
            "queries written in ASCII only",
        )
    } else if target == DEFAULT_ENGLISH_MODEL2VEC_MODEL {
        (
            DEFAULT_MODEL2VEC_MODEL,
            "queries containing non-ASCII letters",
        )
    } else {
        return None;
    };
    if survey
        .other_models
        .iter()
        .any(|(model, count)| model == other && *count > 0)
    {
        return None;
    }
    Some(format!(
        "{routed} route to {other}, which has no vectors in this space; they fall back to BM25 until `brain reindex --model {other}` is run"
    ))
}

/// Outcome of the embedding pass.
struct Progress {
    embedded: usize,
    failed: usize,
    first_errors: Vec<String>,
}

/// Rebuilds every two-hop path from the edges that exist now.
///
/// Purely additive to the graph: it derives paths from edges and touches no
/// memory, so unlike a vector rebuild there is nothing here to approve.
fn rebuild_paths(
    config: &RuntimeConfig,
    dry_run: bool,
    started: Instant,
) -> Result<(), Box<dyn Error>> {
    let storage = crate::open_storage(&config.database)?;
    let before = storage.materialized_path_count(&config.space)?;
    if dry_run {
        return crate::print_json(&json!({
            "status": "planned",
            "space": config.space,
            "paths_now": before,
            "note": "rebuilds two-hop paths from current edges; memory is not touched",
        }));
    }
    let built = storage.rebuild_paths(&config.space)?;
    storage.flush()?;
    crate::print_json(&json!({
        "status": "ok",
        "space": config.space,
        "paths_before": before,
        "paths_after": built,
        "elapsed_ms": started.elapsed().as_millis(),
    }))?;
    Ok(())
}

pub(crate) fn run(config: &RuntimeConfig, args: &ReindexArgs) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let now = now_unix_seconds();
    // Creating a database here would report "0 records, all good" for a typo in
    // --database, which is the opposite of a repair tool's job.
    if !config.database.exists() {
        return Err(format!(
            "no database at {}; run `brain init` first",
            config.database.display()
        )
        .into());
    }
    if args.paths {
        return rebuild_paths(config, args.dry_run, started);
    }
    let connection = open_read_connection(&config.database)?;
    let target_model = resolve_target_model(args.model.as_deref(), config, &connection)?;
    let survey = survey(&connection, &config.space, &target_model, now)?;
    let planned = if args.force {
        survey.active_versions
    } else {
        survey.missing
    };
    let hazard = hazard(&survey, &target_model, args.force);

    if args.dry_run {
        return print_json(&outcome(
            "planned",
            config,
            args,
            &target_model,
            &survey,
            &hazard,
            planned,
            &Progress {
                embedded: 0,
                failed: 0,
                first_errors: Vec::new(),
            },
            started,
        ));
    }

    // With nothing to embed there is nothing to destroy and nothing to waste, so
    // a prompt would only train the user to approve without reading.
    if planned > 0
        && hazard.needs_approval()
        && !approved(args, &target_model, &survey, &hazard, planned)?
    {
        return print_json(&outcome(
            "cancelled",
            config,
            args,
            &target_model,
            &survey,
            &hazard,
            planned,
            &Progress {
                embedded: 0,
                failed: 0,
                first_errors: Vec::new(),
            },
            started,
        ));
    }

    // Loading a model costs about two seconds; there is no reason to pay it
    // just to announce that nothing needs doing.
    let progress = if planned == 0 {
        Progress {
            embedded: 0,
            failed: 0,
            first_errors: Vec::new(),
        }
    } else {
        embed_all(
            config,
            &connection,
            &target_model,
            args.force,
            now,
            planned,
            started,
        )?
    };

    let status = if progress.failed > 0 { "errors" } else { "ok" };
    print_json(&outcome(
        status,
        config,
        args,
        &target_model,
        &survey,
        &hazard,
        planned,
        &progress,
        started,
    ))
}

/// Opens a second, read-only connection for enumeration.
///
/// Writes go through the storage actor; this connection only reads plain
/// tables, never the sqlite-vec virtual ones, so it needs no extension. The
/// busy timeout matters because the reads interleave with the actor's commits
/// for the whole run.
fn open_read_connection(database: &Path) -> Result<Connection, Box<dyn Error>> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(READ_BUSY_TIMEOUT)?;
    Ok(connection)
}

/// Decides which model this rebuild targets.
///
/// The per-text routing used when storing a memory is deliberately not reused:
/// applied here it would declare every English record "missing" a vector under
/// the compact model and rebuild the whole database into a second index nobody
/// asked for. A rebuild has exactly one target.
fn resolve_target_model(
    requested: Option<&str>,
    config: &RuntimeConfig,
    connection: &Connection,
) -> Result<String, Box<dyn Error>> {
    if let Some(model) = requested {
        let model = model.trim();
        if model.is_empty() {
            return Err("--model cannot be empty".into());
        }
        if !model_installed(model, &config.models_dir) {
            return Err(format!(
                "model {model} is not installed in {}; install it before rebuilding the index",
                config.models_dir.display()
            )
            .into());
        }
        return Ok(model.to_owned());
    }

    // The model that already indexed this space is what search reads today, so
    // it stays the default even when another one is also installed.
    let dominant = dominant_model(connection, &config.space)?;
    if let Some(model) = dominant.filter(|model| model_installed(model, &config.models_dir)) {
        return Ok(model);
    }
    for candidate in [DEFAULT_MODEL2VEC_MODEL, DEFAULT_ENGLISH_MODEL2VEC_MODEL] {
        if model_installed(candidate, &config.models_dir) {
            return Ok(candidate.to_owned());
        }
    }
    Err(format!(
        "no embedding model is installed in {}; run `brain model install --model multilingual` first",
        config.models_dir.display()
    )
    .into())
}

/// Returns the model holding the most vectors in one space.
///
/// A failed query is reported rather than folded into "no model yet": swallowing
/// it would silently retarget the rebuild at a default model and quietly write a
/// second index beside the one the space actually uses.
fn dominant_model(
    connection: &Connection,
    space_id: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    let model = connection
        .query_row(
            r#"SELECT model_id FROM embedding_records
               WHERE space_id = ?1
               GROUP BY model_id
               ORDER BY count(*) DESC, model_id
               LIMIT 1"#,
            params![space_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(model)
}

#[cfg(feature = "model2vec")]
fn model_installed(model_id: &str, models_dir: &Path) -> bool {
    // The loader also accepts a directory of model files, which is the only way
    // to use a model that was never published to the hub. Checking the cache
    // alone would refuse a model the backend can load.
    Path::new(model_id).exists() || model_is_cached(model_id, models_dir)
}

#[cfg(not(feature = "model2vec"))]
fn model_installed(_model_id: &str, _models_dir: &Path) -> bool {
    false
}

/// Counts what exists before the rebuild.
///
/// The liveness predicate is copied from the vector-search join on purpose: a
/// version search cannot return is not worth a vector, and one it can return
/// must not be reported as indexed when it is not.
fn survey(
    connection: &Connection,
    space_id: &str,
    model_id: &str,
    now: Timestamp,
) -> Result<Survey, Box<dyn Error>> {
    let (active_versions, already_indexed) = connection.query_row(
        r#"SELECT count(*),
                  count(*) FILTER (WHERE EXISTS (
                      SELECT 1 FROM embedding_records AS r
                      WHERE r.node_version_id = v.row_id AND r.model_id = ?2))
           FROM node_versions AS v
           WHERE v.space_id = ?1
             AND v.valid_from <= ?3
             AND (v.valid_to IS NULL OR ?3 < v.valid_to)
             AND v.recorded_at <= ?3
             AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)"#,
        params![space_id, model_id, now],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let active_versions = count_usize("active node versions", active_versions)?;
    let already_indexed = count_usize("indexed node versions", already_indexed)?;

    // Same liveness predicate as above: a vector on a superseded version is not
    // coverage, and counting it would overstate what another model can answer.
    let mut statement = connection.prepare(
        r#"SELECT r.model_id, count(*)
           FROM embedding_records AS r
           JOIN node_versions AS v ON v.row_id = r.node_version_id
           WHERE r.space_id = ?1
             AND r.model_id <> ?2
             AND v.valid_from <= ?3
             AND (v.valid_to IS NULL OR ?3 < v.valid_to)
             AND v.recorded_at <= ?3
             AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
           GROUP BY r.model_id
           ORDER BY count(*) DESC, r.model_id"#,
    )?;
    let other_models = statement
        .query_map(params![space_id, model_id, now], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(model, count)| Ok((model, count_usize("embeddings per model", count)?)))
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;

    Ok(Survey {
        active_versions,
        already_indexed,
        missing: active_versions.saturating_sub(already_indexed),
        other_models,
    })
}

/// Works out what this run costs the vectors already on disk.
///
/// Vectors under other models are deliberately not counted as a loss: routing
/// is per query text, so they keep answering the queries that select them for
/// the whole run and after it. Calling them casualties would demand consent for
/// a gap-fill that takes nothing away.
fn hazard(survey: &Survey, model_id: &str, force: bool) -> Hazard {
    Hazard {
        overwritten: if force { survey.already_indexed } else { 0 },
        unreachable: !reachable_by_search(model_id),
    }
}

/// Obtains consent before existing vectors are overwritten or stranded.
///
/// A pipe that answers for the user is how data disappears unnoticed, so a
/// non-interactive run without `--yes` refuses instead of assuming yes.
fn approved(
    args: &ReindexArgs,
    model_id: &str,
    survey: &Survey,
    hazard: &Hazard,
    planned: usize,
) -> Result<bool, Box<dyn Error>> {
    let mut stderr = io::stderr();
    writeln!(stderr, "reindex will rewrite the vector index")?;
    writeln!(stderr, "  target model:  {model_id}")?;
    writeln!(
        stderr,
        "  to embed:      {planned} of {} live records",
        survey.active_versions
    )?;
    if hazard.overwritten > 0 {
        writeln!(
            stderr,
            "  overwritten:   {} vectors under {model_id} are recomputed in place and the stored ones are lost",
            hazard.overwritten
        )?;
    }
    if hazard.unreachable {
        writeln!(
            stderr,
            "  unreachable:   search picks its model from the query text and never picks {model_id},"
        )?;
        writeln!(
            stderr,
            "                 so every vector this run writes would sit unread; the models search"
        )?;
        writeln!(
            stderr,
            "                 can reach are {DEFAULT_MODEL2VEC_MODEL} and {DEFAULT_ENGLISH_MODEL2VEC_MODEL}"
        )?;
    }
    for (model, count) in &survey.other_models {
        writeln!(
            stderr,
            "  kept:          {count} vectors under {model} are untouched and keep answering the queries that route to them"
        )?;
    }

    if args.yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        let reason = if hazard.overwritten > 0 {
            "this run destroys stored vectors"
        } else {
            "this run writes vectors search cannot read"
        };
        return Err(format!(
            "refusing without confirmation: {reason}; rerun with --yes, or --dry-run to inspect the plan"
        )
        .into());
    }
    write!(stderr, "proceed? [y/N]: ")?;
    stderr.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// One node version awaiting a vector.
#[cfg(feature = "model2vec")]
struct PendingVersion {
    version_id: i64,
    label: String,
    body: String,
}

/// Reads the next page of versions to embed, ordered by row id.
///
/// Keyset paging rather than OFFSET: the predicate stops matching rows as this
/// run indexes them, and an offset over a shrinking result set silently skips
/// records.
#[cfg(feature = "model2vec")]
fn next_page(
    connection: &Connection,
    space_id: &str,
    model_id: &str,
    now: Timestamp,
    force: bool,
    after: i64,
) -> Result<Vec<PendingVersion>, Box<dyn Error>> {
    let mut statement = connection.prepare_cached(
        r#"SELECT v.row_id, v.label, v.body
           FROM node_versions AS v
           WHERE v.space_id = ?1
             AND v.row_id > ?2
             AND v.valid_from <= ?3
             AND (v.valid_to IS NULL OR ?3 < v.valid_to)
             AND v.recorded_at <= ?3
             AND (v.superseded_at IS NULL OR ?3 < v.superseded_at)
             AND (?4 OR NOT EXISTS (
                     SELECT 1 FROM embedding_records AS r
                     WHERE r.node_version_id = v.row_id AND r.model_id = ?5))
           ORDER BY v.row_id
           LIMIT ?6"#,
    )?;
    let page = statement
        .query_map(
            params![
                space_id,
                after,
                now,
                force,
                model_id,
                i64::try_from(PAGE_SIZE)?
            ],
            |row| {
                Ok(PendingVersion {
                    version_id: row.get(0)?,
                    label: row.get(1)?,
                    body: row.get(2)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(page)
}

#[cfg(feature = "model2vec")]
fn embed_all(
    config: &RuntimeConfig,
    connection: &Connection,
    model_id: &str,
    force: bool,
    now: Timestamp,
    planned: usize,
    started: Instant,
) -> Result<Progress, Box<dyn Error>> {
    let embedder = Model2VecEmbedder::from_pretrained_cached(model_id, &config.models_dir, true)?;
    // Catching a width mismatch here costs one encode; catching it in the
    // storage layer costs one rejected write per record.
    if !matches!(embedder.dimensions(), 256 | 384) {
        return Err(format!(
            "model {model_id} produces {} dimensions; the vector index stores 256 or 384",
            embedder.dimensions()
        )
        .into());
    }

    let storage = open_storage(&config.database)?;
    let attribution = ensure_context(&storage, config, now)?;
    let mut progress = Progress {
        embedded: 0,
        failed: 0,
        first_errors: Vec::new(),
    };
    let mut pending: Vec<NewEmbedding> = Vec::with_capacity(FLUSH_EVERY);
    let mut cursor = 0_i64;

    loop {
        let page = next_page(connection, &config.space, model_id, now, force, cursor)?;
        let Some(last) = page.last() else { break };
        cursor = last.version_id;
        for version in page {
            // The stored text must match what the live indexer embeds, or a
            // rebuilt vector answers a different query than the original.
            let text = format!("{}\n{}", version.label, version.body);
            match embedder.embed(&text) {
                Ok(vector) => pending.push(NewEmbedding {
                    node_version_id: version.version_id,
                    space_id: config.space.clone(),
                    model_id: model_id.to_owned(),
                    vector,
                    embedded_at: now,
                    attribution: attribution.clone(),
                }),
                Err(error) => record_failure(&mut progress, &error.to_string()),
            }
            if pending.len() >= FLUSH_EVERY {
                progress.embedded += commit(&storage, &mut pending)?;
                report_progress(&progress, planned, started);
            }
        }
    }
    progress.embedded += commit(&storage, &mut pending)?;
    report_progress(&progress, planned, started);
    Ok(progress)
}

#[cfg(not(feature = "model2vec"))]
fn embed_all(
    _config: &RuntimeConfig,
    _connection: &Connection,
    model_id: &str,
    _force: bool,
    _now: Timestamp,
    _planned: usize,
    _started: Instant,
) -> Result<Progress, Box<dyn Error>> {
    Err(format!(
        "this build has no embedding backend compiled in, so {model_id} cannot produce vectors; rebuild brain with --features model2vec"
    )
    .into())
}

/// Commits one batch and waits for it to reach disk.
///
/// The durability wait is what makes an interrupted run resumable: every
/// committed batch is already excluded from the next run's page query, and an
/// uncommitted one is simply recomputed.
#[cfg(feature = "model2vec")]
fn commit(
    storage: &brain_core::SqliteStorage,
    pending: &mut Vec<NewEmbedding>,
) -> Result<usize, Box<dyn Error>> {
    if pending.is_empty() {
        return Ok(0);
    }
    let written = pending.len();
    storage.upsert_embeddings(pending)?;
    storage.flush()?;
    pending.clear();
    Ok(written)
}

/// Keeps the first few distinct failures so a systematic fault is visible.
#[cfg(any(feature = "model2vec", test))]
fn record_failure(progress: &mut Progress, message: &str) {
    progress.failed += 1;
    if progress.first_errors.len() < MAX_REPORTED_ERRORS
        && !progress.first_errors.iter().any(|seen| seen == message)
    {
        progress.first_errors.push(message.to_owned());
    }
}

/// Prints one progress line to stderr, leaving stdout to the JSON result.
#[cfg(feature = "model2vec")]
fn report_progress(progress: &Progress, planned: usize, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64();
    let done = progress.embedded + progress.failed;
    let rate = if elapsed > 0.0 {
        f64::from(u32::try_from(done).unwrap_or(u32::MAX)) / elapsed
    } else {
        0.0
    };
    let failed = if progress.failed > 0 {
        format!(", {} failed", progress.failed)
    } else {
        String::new()
    };
    eprintln!("reindex {done}/{planned} ({rate:.0}/s{failed})");
}

#[expect(
    clippy::too_many_arguments,
    reason = "one JSON report over the whole run"
)]
fn outcome(
    status: &str,
    config: &RuntimeConfig,
    args: &ReindexArgs,
    model_id: &str,
    survey: &Survey,
    hazard: &Hazard,
    planned: usize,
    progress: &Progress,
    started: Instant,
) -> Value {
    let other_models: Vec<Value> = survey
        .other_models
        .iter()
        .map(|(model, count)| json!({"model": model, "vectors": count}))
        .collect();
    json!({
        "status": status,
        "database": config.database,
        "space": config.space,
        "model": model_id,
        "mode": if args.force { "rebuild-all" } else { "fill-gaps" },
        "active_versions": survey.active_versions,
        "planned": planned,
        "embedded": progress.embedded,
        "skipped": if args.force { 0 } else { survey.already_indexed },
        "skipped_reason": if args.force {
            Value::Null
        } else {
            json!(format!("already indexed under {model_id}"))
        },
        "failed": progress.failed,
        "first_errors": progress.first_errors,
        "overwritten": hazard.overwritten,
        // Reported so a run under a model search cannot select is visible in the
        // result itself, not only in the prompt the user may have answered once.
        "reachable_by_search": !hazard.unreachable,
        "other_models": other_models,
        "coverage_gap": coverage_gap(survey, model_id).map_or(Value::Null, Value::from),
        "elapsed_ms": started.elapsed().as_millis(),
    })
}

fn count_usize(field: &str, count: i64) -> Result<usize, Box<dyn Error>> {
    usize::try_from(count).map_err(|_| format!("{field} is outside usize: {count}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestReindexCli {
        #[command(flatten)]
        reindex: ReindexArgs,
    }

    #[test]
    fn path_rebuild_rejects_vector_only_flags_instead_of_ignoring_them() {
        for flag in ["--model", "--force", "--yes"] {
            let mut arguments = vec!["reindex", "--paths", flag];
            if flag == "--model" {
                arguments.push("some/model");
            }
            let error = TestReindexCli::try_parse_from(arguments)
                .expect_err("a vector-only flag must conflict with --paths");
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn path_rebuild_keeps_dry_run_available() {
        let parsed = TestReindexCli::try_parse_from(["reindex", "--paths", "--dry-run"])
            .expect("path rebuild may be planned without writing");
        assert!(parsed.reindex.paths);
        assert!(parsed.reindex.dry_run);
    }

    fn survey_of(active: usize, indexed: usize, others: &[(&str, usize)]) -> Survey {
        Survey {
            active_versions: active,
            already_indexed: indexed,
            missing: active.saturating_sub(indexed),
            other_models: others
                .iter()
                .map(|(model, count)| ((*model).to_owned(), *count))
                .collect(),
        }
    }

    #[test]
    fn filling_gaps_touches_nothing_that_already_exists() {
        let hazard = hazard(&survey_of(100, 40, &[]), DEFAULT_MODEL2VEC_MODEL, false);
        assert_eq!(hazard.overwritten, 0);
        assert!(!hazard.unreachable);
        assert!(!hazard.needs_approval());
    }

    #[test]
    fn force_declares_every_existing_vector_as_overwritten() {
        let hazard = hazard(&survey_of(100, 40, &[]), DEFAULT_MODEL2VEC_MODEL, true);
        assert_eq!(hazard.overwritten, 40);
        assert!(hazard.needs_approval());
    }

    /// The two default models serve different query languages at the same time,
    /// so indexing one must not ask permission to "lose" the other.
    #[test]
    fn the_other_routed_model_is_kept_not_treated_as_a_casualty() {
        let survey = survey_of(100, 0, &[(DEFAULT_ENGLISH_MODEL2VEC_MODEL, 100)]);
        let hazard = hazard(&survey, DEFAULT_MODEL2VEC_MODEL, false);
        assert_eq!(hazard.overwritten, 0);
        assert!(!hazard.unreachable);
        assert!(!hazard.needs_approval());
    }

    #[test]
    fn a_model_search_cannot_select_needs_approval_before_the_work_starts() {
        let hazard = hazard(&survey_of(100, 0, &[]), "some/other-model", false);
        assert!(hazard.unreachable);
        assert!(hazard.needs_approval());
    }

    #[test]
    fn a_space_covered_by_only_one_routed_model_reports_the_uncovered_half() {
        let gap = coverage_gap(&survey_of(100, 100, &[]), DEFAULT_MODEL2VEC_MODEL)
            .expect("ASCII-only queries are unserved when only the multilingual model is indexed");
        assert!(gap.contains(DEFAULT_ENGLISH_MODEL2VEC_MODEL));
    }

    #[test]
    fn a_space_covered_by_both_routed_models_reports_no_gap() {
        let survey = survey_of(100, 100, &[(DEFAULT_ENGLISH_MODEL2VEC_MODEL, 100)]);
        assert!(coverage_gap(&survey, DEFAULT_MODEL2VEC_MODEL).is_none());
    }

    #[test]
    fn an_empty_space_reports_no_gap_because_nothing_is_unserved() {
        assert!(coverage_gap(&survey_of(0, 0, &[]), DEFAULT_MODEL2VEC_MODEL).is_none());
    }

    #[test]
    fn only_distinct_failures_are_reported_but_all_are_counted() {
        let mut progress = Progress {
            embedded: 0,
            failed: 0,
            first_errors: Vec::new(),
        };
        for _ in 0..5 {
            record_failure(&mut progress, "tokenizer refused the input");
        }
        record_failure(&mut progress, "backend returned no vector");
        assert_eq!(progress.failed, 6);
        assert_eq!(progress.first_errors.len(), 2);
    }

    #[test]
    fn empty_model_is_rejected_before_any_work() {
        let config = RuntimeConfig {
            database: std::path::PathBuf::from("/nonexistent/brain.sqlite"),
            models_dir: std::path::PathBuf::from("/nonexistent/models"),
            space: "default".to_owned(),
            agent: "test".to_owned(),
            offline: true,
            share_state_dir: None,
        };
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        let error = resolve_target_model(Some("  "), &config, &connection)
            .expect_err("an empty model id is not usable");
        assert!(error.to_string().contains("--model cannot be empty"));
    }

    #[test]
    fn a_missing_model_names_the_directory_it_looked_in() {
        let config = RuntimeConfig {
            database: std::path::PathBuf::from("/nonexistent/brain.sqlite"),
            models_dir: std::path::PathBuf::from("/nonexistent/models"),
            space: "default".to_owned(),
            agent: "test".to_owned(),
            offline: true,
            share_state_dir: None,
        };
        let connection = Connection::open_in_memory().expect("in-memory database opens");
        let error = resolve_target_model(Some("some/model"), &config, &connection)
            .expect_err("an uninstalled model cannot be a rebuild target");
        assert!(error.to_string().contains("/nonexistent/models"));
    }
}
