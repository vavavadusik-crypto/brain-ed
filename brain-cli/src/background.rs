use std::collections::BTreeSet;
#[cfg(feature = "model2vec")]
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(feature = "model2vec")]
use brain_core::Storage;
use brain_core::Timestamp;
use brain_core::embeddings::{
    DEFAULT_ENGLISH_MODEL2VEC_DOWNLOAD_BYTES, DEFAULT_MODEL2VEC_DOWNLOAD_BYTES,
    DEFAULT_MODEL2VEC_MODEL, model_id_for_text,
};
#[cfg(feature = "model2vec")]
use brain_core::embeddings::{DEFAULT_ENGLISH_MODEL2VEC_MODEL, Model2VecEmbedder, model_is_cached};
#[cfg(feature = "model2vec")]
use brain_core::{
    Agent, AsOf, Attribution, HybridSearch, Session, Space, SpaceKind, SqliteStorage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::now_unix_seconds;

pub(crate) const QUEUE_FILE: &str = "embedding-queue.jsonl";
pub(crate) const PROCESSING_FILE: &str = "embedding-queue.processing.jsonl";
pub(crate) const QUEUE_DATABASE: &str = "embedding-queue.sqlite";
const LOCK_FILE: &str = "embedding-worker.lock";
/// Records embedded between forced commits.
const FLUSH_EVERY: usize = 500;

#[derive(Debug, Deserialize, Serialize)]
struct EmbeddingJob {
    database: PathBuf,
    models_dir: PathBuf,
    space: String,
    agent: String,
    node_id: String,
    model_id: String,
    queued_at: Timestamp,
}

/// One row claimed from the durable queue.
///
/// Rows are deleted only after `process_job` succeeds. A killed worker therefore
/// replays a row after restart, and embedding storage's upsert key makes that
/// at-least-once replay idempotent.
struct QueuedEmbeddingJob {
    id: i64,
    job: EmbeddingJob,
}

pub(crate) fn model_download_bytes(model_id: &str) -> u64 {
    if model_id == DEFAULT_MODEL2VEC_MODEL {
        DEFAULT_MODEL2VEC_DOWNLOAD_BYTES
    } else {
        DEFAULT_ENGLISH_MODEL2VEC_DOWNLOAD_BYTES
    }
}

pub(crate) fn install_command(model_id: &str) -> &'static str {
    if model_id == DEFAULT_MODEL2VEC_MODEL {
        "brain model install --model multilingual"
    } else {
        "brain model install --model english"
    }
}

/// Whether one named model is fully present on disk.
pub(crate) fn model_installed(model_id: &str, models_dir: &Path) -> bool {
    #[cfg(feature = "model2vec")]
    {
        model_is_cached(model_id, models_dir)
    }
    #[cfg(not(feature = "model2vec"))]
    {
        let _ = (model_id, models_dir);
        false
    }
}

/// The "install this model" message for a model chosen by the caller.
pub(crate) fn semantic_unavailable_message_for(model_id: &str) -> String {
    let megabytes = model_download_bytes(model_id) / 1_000_000;
    format!(
        "local semantic model {model_id} is not installed ({megabytes} MB). Download it? Run `{}` to approve; semantic search is unavailable until then",
        install_command(model_id)
    )
}

pub(crate) fn enqueue_embedding(
    brain_home: &Path,
    database: &Path,
    models_dir: &Path,
    space: &str,
    agent: &str,
    node_id: &str,
    text: &str,
) -> io::Result<()> {
    let job = EmbeddingJob {
        database: database.to_path_buf(),
        models_dir: models_dir.to_path_buf(),
        space: space.to_owned(),
        agent: agent.to_owned(),
        node_id: node_id.to_owned(),
        model_id: model_id_for_text(text).to_owned(),
        queued_at: now_unix_seconds(),
    };
    enqueue_durable_job(brain_home, &job)
}

/// Counts both the transactional queue and files written by older releases.
/// Jobs waiting for one database, not for every database this machine has.
///
/// The queue is shared across databases, so counting all of it told someone
/// searching a scratch database that 34 vectors were pending — jobs belonging
/// to their main one — and started a worker to index a database they were not
/// using.
#[cfg(any(feature = "model2vec", test))]
pub(crate) fn pending_embedding_jobs(brain_home: &Path, database: &Path) -> usize {
    let wanted = canonical_or_given(database);
    durable_queue_len(brain_home, &wanted)
        .saturating_add(queued_jobs_for(&brain_home.join(QUEUE_FILE), &wanted))
        .saturating_add(queued_jobs_for(&brain_home.join(PROCESSING_FILE), &wanted))
}

/// Everything still queued, for whichever database.
///
/// The worker drains the whole queue, so its own progress report counts the
/// whole queue. Only messages shown to someone working with one database
/// narrow the count to that database.
pub(crate) fn total_pending_embedding_jobs(brain_home: &Path) -> usize {
    durable_queue_total(brain_home)
        .saturating_add(json_line_count(&brain_home.join(QUEUE_FILE)))
        .saturating_add(json_line_count(&brain_home.join(PROCESSING_FILE)))
}

fn durable_queue_total(brain_home: &Path) -> usize {
    let path = brain_home.join(QUEUE_DATABASE);
    if !path.is_file() {
        return 0;
    }
    let Ok(connection) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return 0;
    };
    connection
        .query_row("SELECT count(*) FROM embedding_jobs", [], |row| {
            row.get::<_, i64>(0)
        })
        .ok()
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0)
}

fn json_line_count(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |contents| {
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    })
}

/// A comparable form of a path, falling back to the path itself.
///
/// Canonicalization fails for a database that does not exist yet, which is
/// exactly when the queue is empty anyway.
#[cfg(any(feature = "model2vec", test))]
fn canonical_or_given(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(any(feature = "model2vec", test))]
fn job_targets(job: &EmbeddingJob, database: &Path) -> bool {
    canonical_or_given(&job.database) == database
}

/// Commits one job in a separate SQLite database.
///
/// The old JSONL hand-off could lose a concurrent append: a writer opened the
/// queue, the worker renamed that inode to `processing`, then the writer added
/// bytes after the worker had read it. SQLite serializes insert and delete
/// transactions, so a row is either visible to this worker or to the next one;
/// it can never be appended to a file that is about to be unlinked.
fn enqueue_durable_job(brain_home: &Path, job: &EmbeddingJob) -> io::Result<()> {
    crate::private_fs::ensure_private_home(brain_home)?;
    let connection = open_durable_queue(&brain_home.join(QUEUE_DATABASE), true)?;
    let payload = serde_json::to_string(job).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialize embedding job: {error}"),
        )
    })?;
    connection
        .execute(
            "INSERT INTO embedding_jobs(payload, queued_at) VALUES (?1, ?2)",
            rusqlite::params![payload, job.queued_at],
        )
        .map_err(|error| queue_io("commit embedding job", error))?;
    Ok(())
}

fn open_durable_queue(path: &Path, create: bool) -> io::Result<rusqlite::Connection> {
    let flags = if create {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
    };
    let connection = rusqlite::Connection::open_with_flags(path, flags)
        .map_err(|error| queue_io("open embedding queue", error))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| queue_io("configure embedding queue timeout", error))?;
    if create {
        connection
            .execute_batch(
                "PRAGMA synchronous=FULL;
                 CREATE TABLE IF NOT EXISTS embedding_jobs(
                   id INTEGER PRIMARY KEY,
                   payload TEXT NOT NULL,
                   queued_at INTEGER NOT NULL
                 );",
            )
            .map_err(|error| queue_io("initialize embedding queue", error))?;
    }
    Ok(connection)
}

fn load_durable_jobs(brain_home: &Path) -> io::Result<Vec<QueuedEmbeddingJob>> {
    let path = brain_home.join(QUEUE_DATABASE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let connection = open_durable_queue(&path, false)?;
    let mut statement = connection
        .prepare("SELECT id, payload FROM embedding_jobs ORDER BY id")
        .map_err(|error| queue_io("read embedding queue", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| queue_io("scan embedding queue", error))?;
    let mut jobs = Vec::new();
    for row in rows {
        let (id, payload) = row.map_err(|error| queue_io("decode embedding queue row", error))?;
        let job = serde_json::from_str(&payload).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("embedding queue row {id} is corrupt: {error}"),
            )
        })?;
        jobs.push(QueuedEmbeddingJob { id, job });
    }
    Ok(jobs)
}

fn acknowledge_durable_job(brain_home: &Path, id: i64) -> io::Result<()> {
    let connection = open_durable_queue(&brain_home.join(QUEUE_DATABASE), false)?;
    let deleted = connection
        .execute("DELETE FROM embedding_jobs WHERE id = ?1", [id])
        .map_err(|error| queue_io("acknowledge embedding job", error))?;
    if deleted != 1 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("embedding queue row {id} disappeared before acknowledgement"),
        ));
    }
    Ok(())
}

#[cfg(any(feature = "model2vec", test))]
fn durable_queue_len(brain_home: &Path, database: &Path) -> usize {
    let path = brain_home.join(QUEUE_DATABASE);
    if !path.is_file() {
        return 0;
    }
    let Ok(connection) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return 0;
    };
    let Ok(mut statement) = connection.prepare("SELECT payload FROM embedding_jobs") else {
        return 0;
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return 0;
    };
    rows.filter_map(Result::ok)
        .filter_map(|payload| serde_json::from_str::<EmbeddingJob>(&payload).ok())
        .filter(|job| job_targets(job, database))
        .count()
}

#[cfg(any(feature = "model2vec", test))]
fn queued_jobs_for(path: &Path, database: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |contents| {
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<EmbeddingJob>(line).ok())
            .filter(|job| job_targets(job, database))
            .count()
    })
}

fn queue_io(context: &str, error: rusqlite::Error) -> io::Error {
    io::Error::other(format!("{context}: {error}"))
}

/// The executable that re-runs this process's own image.
///
/// On Linux `current_exe()` is the text of the `/proc/self/exe` link. Once an
/// install renames a new binary over ours, or removes it, that text is
/// `".../brain (deleted)"` and spawning it fails with ENOENT, so a long-lived
/// `brain serve` silently lost its vector worker. `/proc/self/exe` itself still
/// opens the running image, and the spawned child inherits it before exec, so
/// the worker is always the parent's generation. There is deliberately no
/// fallback to the pathname: that file may be a different generation.
#[cfg(target_os = "linux")]
fn own_image() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/proc/self/exe"))
}

/// Elsewhere the pathname is all there is: after the binary is replaced, the
/// worker may be the new generation or fail to start (reported as a warning).
#[cfg(not(target_os = "linux"))]
fn own_image() -> io::Result<PathBuf> {
    std::env::current_exe()
}

pub(crate) fn spawn_worker() -> io::Result<()> {
    Command::new(own_image()?)
        .arg("__worker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

pub(crate) fn run_worker(brain_home: &Path) -> Result<Value, Box<dyn Error>> {
    crate::private_fs::ensure_private_home(brain_home)?;
    let lock_path = brain_home.join(LOCK_FILE);
    let Some(lock) = WorkerLock::acquire(&lock_path)? else {
        return Ok(json!({
            "status": "busy",
            "processed": 0,
            "remaining": total_pending_embedding_jobs(brain_home),
        }));
    };

    let queue_path = brain_home.join(QUEUE_FILE);
    let processing_path = brain_home.join(PROCESSING_FILE);
    let mut processed = 0_usize;
    // Jobs held back only because no model is installed. Reporting them as
    // ordinary "remaining" work hid a total embedding outage behind status ok.
    let mut blocked_no_model = 0_usize;
    // A unit struct only without the `model2vec` feature.
    #[allow(clippy::default_constructed_unit_structs)]
    let mut model_cache = ModelCache::default();
    let mut failed = 0_usize;
    let mut first_errors: Vec<String> = Vec::new();
    let mut missing_models: BTreeSet<String> = BTreeSet::new();

    // New releases enqueue one row per job. Reading a snapshot does not claim
    // or delete anything: a writer may commit after this SELECT and the next
    // worker pass will see it. Only a successful, idempotent embedding upsert
    // acknowledges a row, so a crash is replay rather than loss.
    let mut databases_to_flush = BTreeSet::new();
    let mut attempted_durable_ids = BTreeSet::new();
    for _ in 0..8 {
        let durable_jobs = load_durable_jobs(brain_home)?
            .into_iter()
            .filter(|queued| !attempted_durable_ids.contains(&queued.id))
            .collect::<Vec<_>>();
        if durable_jobs.is_empty() {
            break;
        }
        for queued in durable_jobs {
            attempted_durable_ids.insert(queued.id);
            let job = &queued.job;
            if usable_model(job).is_none() {
                blocked_no_model += 1;
                missing_models.insert(job.model_id.clone());
                continue;
            }
            match process_job(job, &mut model_cache) {
                Ok(true) => {
                    acknowledge_durable_job(brain_home, queued.id)?;
                    processed += 1;
                    databases_to_flush.insert(job.database.clone());
                    if databases_to_flush.len() >= FLUSH_EVERY {
                        for database in &databases_to_flush {
                            flush_database(database);
                        }
                        databases_to_flush.clear();
                    }
                }
                Ok(false) => {
                    blocked_no_model += 1;
                    missing_models.insert(job.model_id.clone());
                }
                Err(error) => {
                    let text = error.to_string();
                    if first_errors.len() < 3 && !first_errors.contains(&text) {
                        first_errors.push(text);
                    }
                    failed += 1;
                }
            }
        }
    }
    for database in &databases_to_flush {
        flush_database(database);
    }

    // Drain JSONL jobs left by pre-transactional releases. No current writer
    // appends here, so the historical rename protocol is now race-free.
    for _ in 0..8 {
        if !processing_path.exists() {
            match fs::rename(&queue_path, &processing_path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }

        let contents = fs::read_to_string(&processing_path)?;
        let mut retry = Vec::new();
        // Flushing after every job forces a commit per record; on ten thousand
        // records that dominates the run. One flush per pass is enough because
        // unflushed work stays in the queue file and is retried.
        let mut pending_since_flush = 0_usize;
        let mut pass_databases = BTreeSet::new();
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            let job = match serde_json::from_str::<EmbeddingJob>(line) {
                Ok(job) => job,
                Err(error) => {
                    let text = format!("legacy embedding queue entry is corrupt: {error}");
                    if first_errors.len() < 3 && !first_errors.contains(&text) {
                        first_errors.push(text);
                    }
                    failed += 1;
                    retry.push(line.to_owned());
                    continue;
                }
            };
            if usable_model(&job).is_none() {
                blocked_no_model += 1;
                missing_models.insert(job.model_id.clone());
                retry.push(line.to_owned());
                continue;
            }
            match process_job(&job, &mut model_cache) {
                Ok(true) => {
                    processed += 1;
                    pending_since_flush += 1;
                    pass_databases.insert(job.database.clone());
                    if pending_since_flush >= FLUSH_EVERY {
                        for database in &pass_databases {
                            flush_database(database);
                        }
                        pass_databases.clear();
                        pending_since_flush = 0;
                    }
                }
                Ok(false) => retry.push(line.to_owned()),
                Err(error) => {
                    // A silently swallowed error here means embeddings stop and
                    // nothing in the output says why.
                    let text = error.to_string();
                    if first_errors.len() < 3 && !first_errors.contains(&text) {
                        first_errors.push(text);
                    }
                    failed += 1;
                    retry.push(line.to_owned());
                }
            }
        }
        if pending_since_flush > 0 {
            for database in &pass_databases {
                flush_database(database);
            }
        }
        fs::remove_file(&processing_path)?;

        if !retry.is_empty() {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&queue_path)?;
            for line in &retry {
                writeln!(file, "{line}")?;
            }
            break;
        }
    }

    drop(lock);
    let remaining = total_pending_embedding_jobs(brain_home);
    if blocked_no_model > 0 {
        let models: Vec<&str> = missing_models.iter().map(String::as_str).collect();
        return Ok(json!({
            "status": "blocked",
            "reason": "no embedding model installed",
            "processed": processed,
            "remaining": remaining,
            "blocked_no_model": blocked_no_model,
            "expected_models": models,
            "hint": "run `brain model install --model multilingual` to enable semantic search",
        }));
    }
    if failed > 0 {
        return Ok(json!({
            "status": "errors",
            "processed": processed,
            "remaining": remaining,
            "failed": failed,
            "first_errors": first_errors,
        }));
    }
    Ok(json!({
        "status": "ok",
        "processed": processed,
        "remaining": remaining,
    }))
}

#[cfg(feature = "model2vec")]
/// Picks a usable model for the job.
///
/// The queued model is preferred, but a job must not stall forever because the
/// user installed a different one: the multilingual model also covers English,
/// so it is an acceptable substitute. Returning `None` means nothing is
/// installed and the caller has to report the job as blocked, not as done.
fn usable_model(job: &EmbeddingJob) -> Option<String> {
    if model_is_cached(&job.model_id, &job.models_dir) {
        return Some(job.model_id.clone());
    }
    // Multilingual can replace the compact English model, never the reverse.
    if job.model_id == DEFAULT_ENGLISH_MODEL2VEC_MODEL
        && model_is_cached(DEFAULT_MODEL2VEC_MODEL, &job.models_dir)
    {
        return Some(DEFAULT_MODEL2VEC_MODEL.to_owned());
    }
    None
}

#[cfg(not(feature = "model2vec"))]
fn usable_model(_job: &EmbeddingJob) -> Option<String> {
    None
}

/// Keeps loaded models alive for the whole worker run.
///
/// Loading `potion-multilingual-128M` costs about 1.9 seconds while encoding a
/// text costs 0.13 milliseconds. Reloading per job turned a quarter-second of
/// real work on 2000 records into an hour of repeated file parsing.
#[cfg(feature = "model2vec")]
#[derive(Default)]
struct ModelCache {
    loaded: HashMap<String, Model2VecEmbedder>,
}

#[cfg(feature = "model2vec")]
impl ModelCache {
    fn get(
        &mut self,
        model_id: &str,
        models_dir: &Path,
    ) -> Result<&Model2VecEmbedder, Box<dyn Error>> {
        if !self.loaded.contains_key(model_id) {
            let embedder = Model2VecEmbedder::from_pretrained_cached(model_id, models_dir, true)?;
            self.loaded.insert(model_id.to_owned(), embedder);
        }
        Ok(self
            .loaded
            .get(model_id)
            .expect("model inserted above is present"))
    }
}

#[cfg(feature = "model2vec")]
/// Commits pending embedding writes for one database.
///
/// Failure is not fatal: the jobs stay in the queue and are retried, so a
/// transient lock must not abort the whole pass.
fn flush_database(database: &Path) {
    if let Ok(storage) = SqliteStorage::open(database) {
        let _ = storage.flush();
    }
}

#[cfg(not(feature = "model2vec"))]
fn flush_database(_database: &Path) {}

#[cfg(feature = "model2vec")]
fn process_job(job: &EmbeddingJob, cache: &mut ModelCache) -> Result<bool, Box<dyn Error>> {
    let Some(model) = usable_model(job) else {
        return Ok(false);
    };
    let storage = SqliteStorage::open(&job.database)?;
    let now = now_unix_seconds().max(job.queued_at);
    let Some(node) = storage.node_as_of(&job.space, &job.node_id, AsOf::new(now, now))? else {
        return Ok(true);
    };
    let session_id = format!("brain-worker:{}:{}", job.agent, std::process::id());
    storage.ensure_context(
        &Agent {
            id: job.agent.clone(),
            display_name: Some(job.agent.clone()),
            metadata_json: r#"{"client":"brain-worker"}"#.to_owned(),
            created_at: now,
        },
        &Space {
            id: job.space.clone(),
            kind: SpaceKind::Project,
            label: job.space.clone(),
            metadata_json: r#"{"client":"brain-worker"}"#.to_owned(),
            created_by_agent_id: job.agent.clone(),
            created_at: now,
        },
        &Session {
            id: session_id.clone(),
            space_id: job.space.clone(),
            agent_id: job.agent.clone(),
            started_at: now,
            ended_at: None,
            metadata_json: r#"{"client":"brain-worker"}"#.to_owned(),
        },
    )?;
    let embedder = cache.get(&model, Path::new(&job.models_dir))?;
    HybridSearch::new(&storage, embedder).index_node(
        &node,
        Attribution {
            agent_id: job.agent.clone(),
            session_id,
            recorded_at: now,
        },
        now,
    )?;
    Ok(true)
}

#[cfg(not(feature = "model2vec"))]
#[derive(Default)]
struct ModelCache;

#[cfg(not(feature = "model2vec"))]
fn process_job(_job: &EmbeddingJob, _cache: &mut ModelCache) -> Result<bool, Box<dyn Error>> {
    Ok(false)
}

/// Exclusive ownership of the embedding worker slot.
///
/// Ownership is an exclusive `File::try_lock` (flock on Linux) on a lock file
/// that persists and is never unlinked. The kernel drops the lock when the
/// owner exits or dies, so there is no stale-file takeover and no pid
/// liveness or pid-reuse guess. Unlinking on release (or taking over a
/// "stale" file) raced: a second worker could lock the old inode while a
/// third created a fresh one, and both ran. The pid inside is diagnostic only.
///
/// Release is an explicit unlock, not just closing the descriptor: a child
/// forked by any thread of this process shares the open file description until
/// it execs (or for its whole life if it inherited the descriptor), and a
/// close alone would leave the slot locked for that long.
struct WorkerLock {
    file: fs::File,
}

impl WorkerLock {
    fn acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = open_lock_file(path)?;
        if !try_lock_exclusive(&file)? {
            return Ok(None);
        }
        // Guard first, so a failed write below still unlocks on drop.
        let mut lock = Self { file };
        lock.file.set_len(0)?;
        writeln!(lock.file, "{}", std::process::id())?;
        Ok(Some(lock))
    }
}

/// Whether a live worker currently holds the lock at `path`.
///
/// A successful probe unlocks explicitly, for the same reason as `Drop`.
#[cfg(test)]
fn lock_is_held(path: &Path) -> bool {
    let Ok(file) = open_lock_file(path) else {
        return false;
    };
    match try_lock_exclusive(&file) {
        Ok(true) => {
            let _ = file.unlock();
            false
        }
        Ok(false) => true,
        Err(_) => false,
    }
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "refusing non-regular worker lock {}",
            path.display()
        )));
    }
    Ok(file)
}

fn try_lock_exclusive(file: &fs::File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

impl Drop for WorkerLock {
    fn drop(&mut self) {
        // Clear the diagnostic pid while still holding the lock, then release
        // it explicitly; the file itself stays.
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_brain_home(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir().join(format!(
            "brain-background-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn missing_model_message_is_actionable_and_sized() {
        let message = semantic_unavailable_message_for(DEFAULT_MODEL2VEC_MODEL);
        assert!(message.contains("507 MB"));
        assert!(message.contains("brain model install --model multilingual"));
    }

    #[test]
    fn transactional_queue_cannot_lose_an_append_between_snapshot_and_ack() {
        let home = temp_brain_home("queue-interleaving");
        let database = home.join("brain.sqlite");
        let models = home.join("models");
        enqueue_embedding(
            &home,
            &database,
            &models,
            "test",
            "agent",
            "first",
            "first job",
        )
        .expect("enqueue first job");

        // This is the exact hand-off window that lost JSONL jobs: the worker
        // has taken its snapshot, then another process commits an append before
        // the worker acknowledges what it saw.
        let first_snapshot = load_durable_jobs(&home).expect("worker snapshot");
        assert_eq!(first_snapshot.len(), 1);
        let first_id = first_snapshot[0].id;
        assert_eq!(first_snapshot[0].job.node_id, "first");

        let writer_home = home.clone();
        let writer_database = database.clone();
        let writer_models = models.clone();
        std::thread::spawn(move || {
            enqueue_embedding(
                &writer_home,
                &writer_database,
                &writer_models,
                "test",
                "agent",
                "second",
                "second job",
            )
        })
        .join()
        .expect("writer thread")
        .expect("commit concurrent append");

        acknowledge_durable_job(&home, first_id).expect("acknowledge first job");
        let second_snapshot = load_durable_jobs(&home).expect("next worker snapshot");
        assert_eq!(second_snapshot.len(), 1, "concurrent append must survive");
        assert_eq!(second_snapshot[0].job.node_id, "second");
        assert_ne!(second_snapshot[0].id, first_id);
        acknowledge_durable_job(&home, second_snapshot[0].id).expect("acknowledge second job");
        assert_eq!(total_pending_embedding_jobs(&home), 0);

        fs::remove_dir_all(home).expect("remove queue fixture");
    }

    #[test]
    fn an_unacknowledged_job_is_replayed_after_a_worker_crash() {
        let home = temp_brain_home("queue-replay");
        enqueue_embedding(
            &home,
            &home.join("brain.sqlite"),
            &home.join("models"),
            "test",
            "agent",
            "replayed",
            "replayed job",
        )
        .expect("enqueue replay fixture");
        let before_crash = load_durable_jobs(&home).expect("first worker snapshot");
        let after_restart = load_durable_jobs(&home).expect("restarted worker snapshot");
        assert_eq!(before_crash.len(), 1);
        assert_eq!(after_restart.len(), 1);
        assert_eq!(before_crash[0].id, after_restart[0].id);
        assert_eq!(after_restart[0].job.node_id, "replayed");

        acknowledge_durable_job(&home, after_restart[0].id).expect("acknowledge replay");
        assert_eq!(total_pending_embedding_jobs(&home), 0);
        fs::remove_dir_all(home).expect("remove replay fixture");
    }

    #[test]
    fn simultaneous_workers_cannot_both_own_the_lock() {
        let home = temp_brain_home("lock-race");
        fs::create_dir_all(&home).expect("home");
        let path = home.join(LOCK_FILE);
        for _ in 0..50 {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    let path = path.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        WorkerLock::acquire(&path).expect("acquire")
                    })
                })
                .collect();
            let locks: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().expect("join"))
                .collect();
            assert_eq!(locks.iter().filter(|lock| lock.is_some()).count(), 1);
            assert!(lock_is_held(&path));
            drop(locks);
            assert!(path.exists(), "the lock file persists");
            assert!(!lock_is_held(&path), "release frees the slot");
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_held_lock_is_preserved_and_a_dead_owner_file_is_reclaimed() {
        let home = temp_brain_home("lock-foreign");
        fs::create_dir_all(&home).expect("home");
        let path = home.join(LOCK_FILE);
        let held = WorkerLock::acquire(&path).expect("acquire").expect("free");
        let before = fs::read_to_string(&path).expect("lock contents");
        let result = run_worker(&home).expect("second worker");
        assert_eq!(result["status"], "busy", "{result}");
        assert_eq!(fs::read_to_string(&path).expect("kept"), before);
        use std::os::unix::fs::MetadataExt;
        let inode = fs::metadata(&path).expect("lock").ino();
        drop(held);
        assert_eq!(
            fs::metadata(&path).expect("kept").ino(),
            inode,
            "never unlinked"
        );
        assert!(!lock_is_held(&path));

        // A crashed owner leaves the file behind but no kernel lock; any pid in
        // it (even a live, reused one) must not block or be trusted.
        fs::write(&path, "1\n").expect("leftover");
        let lock = WorkerLock::acquire(&path)
            .expect("acquire")
            .expect("reclaimed");
        assert_eq!(
            fs::read_to_string(&path).expect("pid").trim(),
            std::process::id().to_string()
        );
        drop(lock);
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn release_frees_the_slot_while_a_child_still_holds_the_descriptor() {
        let home = temp_brain_home("lock-inherited");
        fs::create_dir_all(&home).expect("home");
        let path = home.join(LOCK_FILE);
        let lock = WorkerLock::acquire(&path).expect("acquire").expect("free");
        // A real subprocess holding the locked open file description, as a
        // child forked by a parallel thread would until it execs.
        let inherited = lock.file.try_clone().expect("clone descriptor");
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::from(inherited))
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        assert!(lock_is_held(&path));
        drop(lock);
        let released = !lock_is_held(&path);
        let next = WorkerLock::acquire(&path).expect("acquire again");
        let _ = child.kill();
        let _ = child.wait();
        assert!(released, "dropping the guard must release the slot at once");
        assert!(
            next.is_some(),
            "a new worker gets the slot while the child lives"
        );
        drop(next);
        assert!(path.exists(), "the lock file persists");
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_lock_is_refused() {
        let home = temp_brain_home("lock-symlink");
        fs::create_dir_all(&home).expect("home");
        let target = home.join("target");
        fs::write(&target, "keep").expect("target");
        std::os::unix::fs::symlink(&target, home.join(LOCK_FILE)).expect("symlink");
        assert!(WorkerLock::acquire(&home.join(LOCK_FILE)).is_err());
        assert_eq!(fs::read_to_string(&target).expect("target"), "keep");
        let _ = fs::remove_dir_all(&home);
    }
}
