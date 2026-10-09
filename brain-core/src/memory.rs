//! User-memory write semantics, metadata, and contradiction detection.

use std::collections::HashSet;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::embeddings::Embedder;
use crate::{
    AsOf, Attribution, NewEmbedding, NewNode, Node, NodeKind, Storage, StorageError, Timestamp,
};

const MAX_DUPLICATE_SCAN: usize = 5_000;
const MAX_EMBEDDED_COMPARISONS: usize = 20;
const DUPLICATE_SIMILARITY: f64 = 0.92;
const CONTRADICTION_THRESHOLD: f64 = 0.70;
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Product-level memory categories exposed by the CLI and MCP surface.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MemoryKind {
    /// A durable statement about the world or project.
    Fact,
    /// A chosen direction and its rationale.
    Decision,
    /// Work that remains to be done.
    Task,
    /// A failure mode or lesson that should not be repeated.
    Gotcha,
    /// A concrete error, failed attempt, or diagnostic outcome.
    Error,
    /// Free-form context that does not fit a stricter category.
    Note,
}

impl MemoryKind {
    /// Stable lowercase representation persisted in metadata.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Decision => "decision",
            Self::Task => "task",
            Self::Gotcha => "gotcha",
            Self::Error => "error",
            Self::Note => "note",
        }
    }

    /// Canonical storage category used for this product-level memory kind.
    #[must_use]
    pub const fn node_kind(self) -> NodeKind {
        match self {
            Self::Decision => NodeKind::Decision,
            Self::Gotcha | Self::Error => NodeKind::Error,
            Self::Fact | Self::Task | Self::Note => NodeKind::Memory,
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MemoryKind {
    type Err = MemoryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fact" => Ok(Self::Fact),
            "decision" => Ok(Self::Decision),
            "task" => Ok(Self::Task),
            "gotcha" => Ok(Self::Gotcha),
            "error" => Ok(Self::Error),
            "note" => Ok(Self::Note),
            _ => Err(MemoryError::InvalidInput(format!(
                "unknown memory kind {value:?}; expected fact, decision, task, gotcha, error, or note"
            ))),
        }
    }
}

/// Input needed to persist one attributed, bi-temporal memory.
#[derive(Clone, Debug, PartialEq)]
pub struct RememberRequest {
    /// Full memory text.
    pub text: String,
    /// Product-level record category.
    pub kind: MemoryKind,
    /// User-provided retrieval tags.
    pub tags: Vec<String>,
    /// Isolated namespace.
    pub space_id: String,
    /// Optional source path or URL.
    pub source: Option<String>,
    /// First outside-world instant at which the fact is true.
    pub valid_from: Timestamp,
    /// Optional exclusive end of outside-world validity.
    pub valid_to: Option<Timestamp>,
    /// Agent/session/system-time attribution.
    pub attribution: Attribution,
    /// Reliability of the source in the inclusive range zero through one.
    pub source_weight: f64,
}

/// Existing record reported as a likely duplicate.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryDuplicate {
    /// Existing stable memory ID.
    pub id: String,
    /// Best lexical or Model2Vec similarity observed.
    pub similarity: f64,
}

/// Existing record reported as potentially contradictory.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryContradiction {
    /// Existing stable memory ID.
    pub existing_id: String,
    /// Bounded excerpt of existing content.
    pub existing_text: String,
    /// Composite similarity/opposition/subject score.
    pub score: f64,
    /// Safe manual resolution guidance.
    pub suggestion: String,
}

/// Successful memory write plus non-fatal retrieval/indexing diagnostics.
#[derive(Clone, Debug, PartialEq)]
pub struct RememberOutput {
    /// New stable memory ID.
    pub id: String,
    /// Likely duplicates that were not silently discarded.
    pub duplicates: Vec<MemoryDuplicate>,
    /// Potential contradictions that require a human/agent decision.
    pub contradictions: Vec<MemoryContradiction>,
    /// Whether a vector was persisted for this exact node version.
    pub vector_indexed: bool,
    /// Honest degradation notices.
    pub warnings: Vec<String>,
}

/// Parsed product view of a stored memory node.
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryRecord {
    /// Canonical bi-temporal node view.
    pub node: Node,
    /// Product-level kind from metadata.
    pub kind: MemoryKind,
    /// Retrieval tags from metadata.
    pub tags: Vec<String>,
    /// Automatically extracted exact-match entity terms.
    pub entities: Vec<String>,
}

impl MemoryRecord {
    /// Decodes a memory node written by [`MemoryService`].
    ///
    /// # Errors
    ///
    /// Returns an error when metadata is malformed or not marked as memory.
    pub fn from_node(node: Node) -> Result<Self, MemoryError> {
        let metadata: serde_json::Value = serde_json::from_str(&node.metadata_json)
            .map_err(|error| MemoryError::Metadata(error.to_string()))?;
        if metadata
            .get("brain_memory")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(MemoryError::Metadata(
                "node is not marked as a user memory".to_owned(),
            ));
        }
        let kind = metadata
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| MemoryError::Metadata("memory kind is missing".to_owned()))?
            .parse()?;
        Ok(Self {
            tags: string_array(&metadata, "tags"),
            entities: string_array(&metadata, "entities"),
            node,
            kind,
        })
    }
}

/// Error that prevents a requested memory operation from completing.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// Caller input violates the memory contract.
    #[error("invalid memory input: {0}")]
    InvalidInput(String),
    /// Persisted memory metadata is corrupt or incomplete.
    #[error("invalid memory metadata: {0}")]
    Metadata(String),
    /// Storage failed before the canonical memory write completed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Storage-neutral user-memory service with an optional runtime embedder.
pub struct MemoryService<'a, S: ?Sized> {
    storage: &'a S,
    embedder: Option<&'a dyn Embedder>,
    vector_warning: Option<String>,
}

impl<'a, S> MemoryService<'a, S>
where
    S: Storage + ?Sized,
{
    /// Creates a fully hybrid memory writer.
    #[must_use]
    pub fn new<E: Embedder>(storage: &'a S, embedder: &'a E) -> Self {
        Self {
            storage,
            embedder: Some(embedder),
            vector_warning: None,
        }
    }

    /// Creates a lexical-only writer with a caller-visible reason.
    #[must_use]
    pub fn without_vectors(storage: &'a S, reason: impl Into<String>) -> Self {
        Self {
            storage,
            embedder: None,
            vector_warning: Some(reason.into()),
        }
    }

    /// Creates a service around an optional runtime-loaded backend.
    #[must_use]
    pub fn with_optional_embedder(
        storage: &'a S,
        embedder: Option<&'a dyn Embedder>,
        vector_warning: Option<String>,
    ) -> Self {
        Self {
            storage,
            embedder,
            vector_warning,
        }
    }

    /// Stores one memory and reports duplicates/contradictions immediately.
    ///
    /// Embedding failures after the canonical write are warnings: the record
    /// remains searchable through FTS5 and exact entity matching.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input or failure of the canonical SQLite write.
    pub fn remember(&self, request: &RememberRequest) -> Result<RememberOutput, MemoryError> {
        validate_request(request)?;
        let as_of = AsOf::new(request.valid_from, request.attribution.recorded_at);
        let existing =
            self.storage
                .list_memories(&request.space_id, None, as_of, MAX_DUPLICATE_SCAN)?;

        let mut warnings = self.vector_warning.clone().into_iter().collect::<Vec<_>>();
        if self.embedder.is_none() && warnings.is_empty() {
            warnings.push(
                "embedding model unavailable; memory stored for BM25 + exact search only"
                    .to_owned(),
            );
        }
        let mut new_vector = None;
        if let Some(embedder) = self.embedder {
            match embedder.embed(&request.text) {
                Ok(vector) if vector.len() == embedder.dimensions() => new_vector = Some(vector),
                Ok(vector) => warnings.push(format!(
                    "embedding backend returned {} dimensions; expected {}; memory stored without a vector",
                    vector.len(),
                    embedder.dimensions()
                )),
                Err(error) => warnings.push(format!(
                    "embedding generation failed ({error}); memory stored for BM25 + exact search only"
                )),
            }
        }

        let tags = normalize_tags(&request.tags)?;
        let entities = extract_entities(&request.text, &tags);
        let (mut duplicates, mut contradictions) = detect_conflicts(
            &request.text,
            &tags,
            &existing,
            self.embedder,
            new_vector.as_deref(),
        );
        duplicates.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| left.id.cmp(&right.id))
        });
        contradictions.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.existing_id.cmp(&right.existing_id))
        });

        let id = memory_id(&request.text);
        let metadata_json = serde_json::json!({
            "brain_memory": true,
            "kind": request.kind.as_str(),
            "tags": tags,
            "entities": entities,
        })
        .to_string();
        self.storage.insert_node(&NewNode {
            id: id.clone(),
            space_id: request.space_id.clone(),
            kind: request.kind.node_kind(),
            label: memory_label(&request.text),
            body: request.text.trim().to_owned(),
            source: request.source.clone(),
            metadata_json,
            valid_from: request.valid_from,
            valid_to: request.valid_to,
            attribution: request.attribution.clone(),
            salience: request.source_weight,
            source_weight: request.source_weight,
        })?;

        let mut vector_indexed = false;
        if let (Some(embedder), Some(vector)) = (self.embedder, new_vector) {
            let node = self
                .storage
                .node_as_of(&request.space_id, &id, as_of)?
                .ok_or_else(|| StorageError::NotFound(format!("new memory {id:?}")))?;
            let embedding = NewEmbedding {
                node_version_id: node.version_id,
                space_id: request.space_id.clone(),
                model_id: embedder.model_id().to_owned(),
                vector,
                embedded_at: request.attribution.recorded_at,
                attribution: request.attribution.clone(),
            };
            match self.storage.upsert_embedding(&embedding) {
                Ok(()) => vector_indexed = true,
                Err(error) => warnings.push(format!(
                    "vector persistence failed ({error}); memory remains searchable through BM25 + exact matches"
                )),
            }
        }

        Ok(RememberOutput {
            id,
            duplicates,
            contradictions,
            vector_indexed,
            warnings,
        })
    }
}

fn validate_request(request: &RememberRequest) -> Result<(), MemoryError> {
    if request.text.trim().is_empty() {
        return Err(MemoryError::InvalidInput("text cannot be empty".to_owned()));
    }
    if request.space_id.trim().is_empty()
        || request.attribution.agent_id.trim().is_empty()
        || request.attribution.session_id.trim().is_empty()
    {
        return Err(MemoryError::InvalidInput(
            "space, agent, and session identifiers cannot be empty".to_owned(),
        ));
    }
    if request
        .valid_to
        .is_some_and(|valid_to| valid_to <= request.valid_from)
    {
        return Err(MemoryError::InvalidInput(
            "valid_to must be later than valid_from".to_owned(),
        ));
    }
    if !request.source_weight.is_finite() || !(0.0..=1.0).contains(&request.source_weight) {
        return Err(MemoryError::InvalidInput(
            "source_weight must be between zero and one".to_owned(),
        ));
    }
    Ok(())
}

fn detect_conflicts(
    text: &str,
    tags: &[String],
    existing: &[Node],
    embedder: Option<&dyn Embedder>,
    new_vector: Option<&[f32]>,
) -> (Vec<MemoryDuplicate>, Vec<MemoryContradiction>) {
    let normalized = normalize_text(text);
    let new_tokens = significant_tokens(text);
    let mut duplicates = Vec::new();
    let mut contradictions = Vec::new();
    let mut embedded_comparisons = 0usize;

    for candidate in existing {
        let lexical = if normalized == normalize_text(&candidate.body) {
            1.0
        } else {
            dice_similarity(&new_tokens, &significant_tokens(&candidate.body))
        };
        let subject = subject_overlap(tags, &new_tokens, candidate);
        let semantic = if lexical >= 0.25 && embedded_comparisons < MAX_EMBEDDED_COMPARISONS {
            match (embedder, new_vector) {
                (Some(embedder), Some(new_vector)) => {
                    embedded_comparisons += 1;
                    embedder
                        .embed(&candidate.body)
                        .ok()
                        .filter(|vector| vector.len() == new_vector.len())
                        .map_or(lexical, |vector| {
                            cosine_similarity(new_vector, &vector).max(lexical)
                        })
                }
                _ => lexical,
            }
        } else {
            lexical
        };

        if semantic >= DUPLICATE_SIMILARITY {
            duplicates.push(MemoryDuplicate {
                id: candidate.id.clone(),
                similarity: semantic,
            });
            continue;
        }
        let opposition = opposition_signal(text, &candidate.body);
        let contradiction_score = semantic * 0.4 + opposition * 0.5 + subject * 0.1;
        if contradiction_score > CONTRADICTION_THRESHOLD {
            contradictions.push(MemoryContradiction {
                existing_id: candidate.id.clone(),
                existing_text: truncate_chars(&candidate.body, 120),
                score: contradiction_score,
                suggestion: "Close the existing record with memory delete/update, or keep both when their contexts differ."
                    .to_owned(),
            });
        }
    }
    (duplicates, contradictions)
}

fn subject_overlap(tags: &[String], tokens: &HashSet<String>, candidate: &Node) -> f64 {
    let metadata = serde_json::from_str::<serde_json::Value>(&candidate.metadata_json).ok();
    let existing_tags = metadata
        .as_ref()
        .map_or_else(Vec::new, |value| string_array(value, "tags"));
    if tags
        .iter()
        .any(|tag| existing_tags.iter().any(|existing| existing == tag))
    {
        return 1.0;
    }
    let existing_tokens = significant_tokens(&candidate.body);
    let shared = tokens.intersection(&existing_tokens).count();
    if shared >= 2 { 1.0 } else { 0.0 }
}

fn opposition_signal(left: &str, right: &str) -> f64 {
    let left = normalize_text(left);
    let right = normalize_text(right);
    const EXCLUSIVE: &[(&str, &str)] = &[
        ("websockets", "sse"),
        ("postgresql", "mysql"),
        ("enabled", "disabled"),
        ("enable", "disable"),
        ("использовать", "не использовать"),
        ("включить", "отключить"),
    ];
    if EXCLUSIVE.iter().any(|(first, second)| {
        (left.contains(first) && right.contains(second))
            || (left.contains(second) && right.contains(first))
    }) {
        return 1.0;
    }
    let left_negated = contains_negation(&left);
    let right_negated = contains_negation(&right);
    if left_negated != right_negated {
        1.0
    } else {
        0.0
    }
}

fn contains_negation(text: &str) -> bool {
    let padded = format!(" {text} ");
    [
        " not ",
        " no longer ",
        " never ",
        " dropped ",
        " deprecated ",
        " disabled ",
        " не ",
        " больше не ",
        " запрещено ",
        " отключено ",
    ]
    .iter()
    .any(|marker| padded.contains(marker))
}

/// Normalize one agent-supplied tag for storage.
///
/// Reserved scope markers (`project:<name>`, `scope:space`) are serialized
/// with their colon intact so retrieval layers (map packets, scoped reads)
/// can match on the prefix; `project:<name>` must already pass the same
/// project-name validator the coordinator uses and keeps canonical casing —
/// an invalid project marker is rejected, never silently rewritten or
/// lowercased. All other tags keep the historic freeform behaviour:
/// lowercased, non-alphanumeric coerced to spaces, whitespace collapsed.
fn normalize_tag(tag: &str) -> Result<Option<String>, MemoryError> {
    let trimmed = tag.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Some(rest) = trimmed.strip_prefix("scope:") {
        return if rest.trim() == "space" {
            Ok(Some("scope:space".to_owned()))
        } else {
            Err(MemoryError::InvalidInput(format!(
                "unknown scope marker scope:{rest}; only scope:space is supported"
            )))
        };
    }
    if let Some(rest) = trimmed.strip_prefix("project:") {
        let project = rest.trim();
        if project.is_empty() {
            return Err(MemoryError::InvalidInput(
                "project marker requires a project name".to_owned(),
            ));
        }
        // Metadata is an agent-asserted classification, not authority: a
        // malformed marker never widens scope, it is rejected outright.
        if !crate::coordination::is_valid_project(project) {
            return Err(MemoryError::InvalidInput(format!(
                "project:{project} is not a valid project name (1..=100 chars of [A-Za-z0-9._-/] without empty, '.' or '..' segments)"
            )));
        }
        return Ok(Some(format!("project:{project}")));
    }
    let normalized = normalize_text(trimmed);
    if normalized.is_empty() {
        Ok(None)
    } else {
        Ok(Some(normalized))
    }
}

fn normalize_tags(tags: &[String]) -> Result<Vec<String>, MemoryError> {
    let mut tags = tags
        .iter()
        .map(|tag| normalize_tag(tag))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    Ok(tags)
}

fn extract_entities(text: &str, tags: &[String]) -> Vec<String> {
    let mut entities = tags.to_vec();
    entities.extend(text.split_whitespace().filter_map(|raw| {
        let token = raw
            .trim_matches(|character: char| !character.is_alphanumeric() && character != '_')
            .to_owned();
        let has_uppercase = token.chars().any(char::is_uppercase);
        let has_lowercase = token.chars().any(char::is_lowercase);
        let acronym = has_uppercase && !has_lowercase;
        ((acronym && token.chars().count() >= 2)
            || (has_uppercase && has_lowercase && token.chars().count() >= 4))
            .then_some(token)
    }));
    entities.sort_by_key(|entity| entity.to_lowercase());
    entities.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    entities.truncate(32);
    entities
}

fn significant_tokens(text: &str) -> HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of",
        "on", "or", "the", "to", "use", "with", "и", "в", "во", "на", "по", "с", "со", "для",
        "это", "как", "что", "из", "к", "у",
    ];
    normalize_text(text)
        .split_whitespace()
        .filter(|token| token.chars().count() >= 2 && !STOPWORDS.contains(token))
        .map(str::to_owned)
        .collect()
}

fn dice_similarity(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    2.0 * left.intersection(right).count() as f64 / (left.len() + right.len()) as f64
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let (mut dot, mut left_norm, mut right_norm) = (0.0f64, 0.0f64, 0.0f64);
    for (&left, &right) in left.iter().zip(right) {
        let left = f64::from(left);
        let right = f64::from(right);
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator <= f64::EPSILON {
        0.0
    } else {
        dot / denominator
    }
}

fn normalize_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn memory_label(text: &str) -> String {
    truncate_chars(
        &text
            .split_whitespace()
            .take(12)
            .collect::<Vec<_>>()
            .join(" "),
        96,
    )
}

fn memory_id(text: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = DefaultHasher::new();
    nanos.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    counter.hash(&mut hasher);
    text.hash(&mut hasher);
    format!("mem_{:016x}", hasher.finish())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut truncated = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        truncated.push('…');
    }
    truncated
}

fn string_array(metadata: &serde_json::Value, field: &str) -> Vec<String> {
    metadata
        .get(field)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Agent, Session, Space, SpaceKind, SqliteStorage};

    const SPACE_ID: &str = "memory-service-space";
    const AGENT_ID: &str = "memory-service-agent";
    const SESSION_ID: &str = "memory-service-session";

    fn setup() -> SqliteStorage {
        let storage = SqliteStorage::open_in_memory().expect("open memory database");
        storage
            .ensure_context(
                &Agent {
                    id: AGENT_ID.to_owned(),
                    display_name: None,
                    metadata_json: "{}".to_owned(),
                    created_at: 1,
                },
                &Space {
                    id: SPACE_ID.to_owned(),
                    kind: SpaceKind::Project,
                    label: "Memory tests".to_owned(),
                    metadata_json: "{}".to_owned(),
                    created_by_agent_id: AGENT_ID.to_owned(),
                    created_at: 1,
                },
                &Session {
                    id: SESSION_ID.to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    agent_id: AGENT_ID.to_owned(),
                    started_at: 1,
                    ended_at: None,
                    metadata_json: "{}".to_owned(),
                },
            )
            .expect("create memory context");
        storage
    }

    fn request(text: &str, recorded_at: Timestamp) -> RememberRequest {
        RememberRequest {
            text: text.to_owned(),
            kind: MemoryKind::Decision,
            tags: vec!["realtime".to_owned()],
            space_id: SPACE_ID.to_owned(),
            source: None,
            valid_from: recorded_at,
            valid_to: None,
            attribution: Attribution {
                agent_id: AGENT_ID.to_owned(),
                session_id: SESSION_ID.to_owned(),
                recorded_at,
            },
            source_weight: 0.8,
        }
    }

    #[test]
    fn remember_reports_duplicate_and_persists_metadata_without_vectors() {
        let storage = setup();
        let service = MemoryService::without_vectors(&storage, "offline test");
        let first = service
            .remember(&request("Use SSE for realtime events", 10))
            .expect("remember first fact");
        assert!(!first.vector_indexed);
        assert!(!first.warnings.is_empty());

        let second = service
            .remember(&request("Use SSE for realtime events", 11))
            .expect("remember duplicate");
        assert_eq!(second.duplicates.len(), 1);
        assert_eq!(second.duplicates[0].id, first.id);
        assert_eq!(second.duplicates[0].similarity, 1.0);

        let stored = storage
            .node_as_of(SPACE_ID, &first.id, AsOf::new(20, 20))
            .expect("read memory")
            .expect("memory exists");
        let record = MemoryRecord::from_node(stored).expect("decode memory metadata");
        assert_eq!(record.kind, MemoryKind::Decision);
        assert_eq!(record.tags, vec!["realtime"]);
    }

    #[test]
    fn remember_flags_mutually_exclusive_decisions() {
        let storage = setup();
        let service = MemoryService::without_vectors(&storage, "offline test");
        let first = service
            .remember(&request("Use WebSockets for realtime events", 10))
            .expect("remember old decision");
        let second = service
            .remember(&request("Use SSE for realtime events", 11))
            .expect("remember new decision");

        assert_eq!(second.contradictions.len(), 1);
        assert_eq!(second.contradictions[0].existing_id, first.id);
        assert!(second.contradictions[0].score > CONTRADICTION_THRESHOLD);
    }

    #[test]
    fn memory_kind_parser_is_strict() {
        assert_eq!("gotcha".parse::<MemoryKind>().unwrap(), MemoryKind::Gotcha);
        assert_eq!("error".parse::<MemoryKind>().unwrap(), MemoryKind::Error);
        assert!("code".parse::<MemoryKind>().is_err());
    }

    #[test]
    fn error_kind_round_trips_through_the_database_schema() {
        let storage = setup();
        let service = MemoryService::without_vectors(&storage, "offline test");
        let mut error = request("Laptop overheated to 98 degrees", 10);
        error.kind = MemoryKind::Error;
        let output = service.remember(&error).expect("remember error");
        let stored = storage
            .node_as_of(SPACE_ID, &output.id, AsOf::new(20, 20))
            .expect("read error")
            .expect("error exists");
        assert_eq!(stored.kind, NodeKind::Error);
        assert_eq!(
            MemoryRecord::from_node(stored).expect("decode error").kind,
            MemoryKind::Error
        );
    }
}
