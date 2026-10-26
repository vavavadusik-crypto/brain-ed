//! Reusable templates reconstructed from past work, with mandatory provenance.
//!
//! Storing and searching experience is not the same as reusing it. This module
//! answers two questions the rest of the engine cannot: which past work resembles
//! the task at hand, and which of its steps, decisions, and pitfalls are backed by
//! enough evidence to hand back as a plan.
//!
//! The module never writes prose of its own. Every emitted step quotes a stored
//! record verbatim and carries that record's identifier and date, so a caller can
//! always answer "who says so, and when". When the evidence is thin the report says
//! so instead of padding the gap with plausible-sounding advice: a confident
//! template built from three records is worse than an honest refusal.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::DateTime;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{AsOf, NodeKind, Timestamp};

/// Machine-readable schema version for `similar` and `template` JSON reports.
const REPORT_SCHEMA_VERSION: u32 = 1;

/// Node categories that carry reusable experience.
const EXPERIENCE_KINDS: [NodeKind; 3] = [NodeKind::Decision, NodeKind::Error, NodeKind::Memory];
/// Lexical candidates pulled before graph expansion.
const DEFAULT_MAX_SEEDS: usize = 120;
/// Episodes reported by `similar`.
const DEFAULT_MAX_EPISODES: usize = 8;
/// Independent episodes below which no template is produced at all.
const MIN_EPISODES: usize = 2;
/// Sourced records below which no template is produced at all.
const MIN_RECORDS: usize = 3;
/// Episodes below which evidence is thin and a template is refused.
const THIN_EPISODES: usize = 3;
/// Records below which evidence is thin and a template is refused.
const THIN_RECORDS: usize = 6;
/// Records sharing one timestamp above which that timestamp is an import artifact.
const BULK_IMPORT_THRESHOLD: usize = 25;
/// Nodes sharing one exact label above which that label is a structural header.
///
/// Measured on a 10590-node archive: labels such as "Reusable knowledge" (68
/// nodes), "Память" (58) and "User preferences" (52) are section titles emitted
/// by the writer, not lessons. They merge into a single step with dozens of
/// sources across dozens of episodes, so without this bound the most
/// confidently "proven" step of every template is a heading. Genuine repeated
/// topics on that archive top out at eight nodes, which this leaves untouched.
const BOILERPLATE_LABEL_THRESHOLD: usize = 10;
/// Dice overlap above which two statements are treated as the same step.
const STEP_MERGE_SIMILARITY: f64 = 0.60;
/// Neighbours pulled per seed before an episode stops growing.
const MAX_NEIGHBOUR_FANOUT: usize = 40;
/// Characters kept from a record body when it is quoted.
const EXCERPT_CHARS: usize = 240;
/// Relevance retained by a record reached through the graph rather than the query.
const NEIGHBOUR_DECAY: f64 = 0.45;
/// Share of the query's terms a record must carry to count as evidence.
///
/// The seed query joins terms with OR so that recall survives wording changes,
/// but OR alone makes every query look answered: on a real archive
/// "kubernetes ingress certificate rotation" matched 24 steps' worth of records
/// through the single word "rotation", and the report called that sufficient.
/// A record has to carry a real share of what was asked to be treated as
/// experience about it.
const REQUIRED_TERM_SHARE: f64 = 0.5;

/// Failure that prevents an experience lookup from completing.
#[derive(Debug, Error)]
pub enum TemplateError {
    /// The caller supplied no searchable text or no space identifier.
    #[error("invalid experience query: {0}")]
    InvalidQuery(String),

    /// Reading past experience from the database failed.
    #[error("cannot read past experience: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for TemplateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

/// Knobs controlling how much past work is considered and how strict the
/// evidence bar is.
#[derive(Clone, Debug, PartialEq)]
pub struct ExperienceOptions {
    /// Lexical candidates pulled before graph expansion.
    pub max_seeds: usize,
    /// Episodes returned to the caller.
    pub max_episodes: usize,
    /// Independent episodes below which no template is produced.
    pub min_episodes: usize,
    /// Sourced records below which no template is produced.
    pub min_records: usize,
    /// Records sharing one timestamp above which that timestamp is an import artifact.
    pub bulk_import_threshold: usize,
    /// Nodes sharing one exact label above which that label is a structural header.
    pub boilerplate_label_threshold: usize,
    /// Extra seeds backed by a caller-side exact or vector signal.
    ///
    /// The core reads SQLite directly and has no embedding backend of its own, so
    /// a CLI that already loaded one passes its hits here instead of losing them.
    pub trusted_seeds: Vec<ExperienceSeed>,
    /// Full bi-temporal coordinate used for nodes and edges.
    pub as_of: AsOf,
    /// Caller-side degradation notices that must survive into JSON.
    pub degradation_warnings: Vec<String>,
}

impl ExperienceOptions {
    /// Builds default options anchored at one system time.
    #[must_use]
    pub fn at(now: Timestamp) -> Self {
        Self {
            max_seeds: DEFAULT_MAX_SEEDS,
            max_episodes: DEFAULT_MAX_EPISODES,
            min_episodes: MIN_EPISODES,
            min_records: MIN_RECORDS,
            bulk_import_threshold: BULK_IMPORT_THRESHOLD,
            boilerplate_label_threshold: BOILERPLATE_LABEL_THRESHOLD,
            trusted_seeds: Vec::new(),
            as_of: AsOf::new(now, now),
            degradation_warnings: Vec::new(),
        }
    }
}

/// A direct signal trusted to seed experience graph expansion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExperienceSeedSignal {
    /// A semantic vector match survived hybrid-search quality gates.
    Vector,
    /// An exact entity or identifier match survived hybrid-search quality gates.
    Exact,
}

impl ExperienceSeedSignal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Exact => "exact",
        }
    }
}

/// One caller-supplied seed with its measured relevance and signal provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ExperienceSeed {
    /// Stable logical node identifier.
    pub node_id: String,
    /// Caller-side fused relevance in the inclusive range zero through one.
    pub relevance: f64,
    /// Signal that makes this seed safe to expand.
    pub signal: ExperienceSeedSignal,
}

/// How much a record's date can be trusted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DateConfidence {
    /// The timestamp is distinct enough to describe when the work happened.
    Observed,
    /// Many records share this exact timestamp, so it dates an import, not the work.
    BulkImport,
}

impl DateConfidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::BulkImport => "bulk-import",
        }
    }
}

/// One stored record cited as evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ExperienceRecord {
    /// Stable logical node identifier.
    pub node_id: String,
    /// Owning namespace.
    pub space_id: String,
    /// Physical revision selected at `as_of`.
    pub revision: i64,
    /// Stored node category.
    pub kind: NodeKind,
    /// Short label, quoted verbatim from storage.
    pub label: String,
    /// Bounded body excerpt, quoted verbatim from storage.
    pub excerpt: String,
    /// Optional original file path or URL.
    pub source: Option<String>,
    /// World time at which the record became valid.
    pub valid_from: Timestamp,
    /// World time at which the record stopped being valid, when it was closed.
    pub valid_to: Option<Timestamp>,
    /// System time at which Brain learned this revision.
    pub recorded_at: Timestamp,
    /// System-time end of this revision, normally absent at the selected coordinate.
    pub superseded_at: Option<Timestamp>,
    /// Agent that authored this revision.
    pub agent_id: String,
    /// Session that authored this revision.
    pub session_id: String,
    /// UTC calendar date derived from `valid_from`.
    pub date: String,
    /// Whether that date describes the work or an import.
    pub date_confidence: DateConfidence,
    /// Set when this label recurs across the archive often enough to be a
    /// structural header rather than a lesson; holds how many nodes share it.
    pub label_repeats: Option<usize>,
    /// How many of the query's terms this record's own text carries.
    pub terms_matched: usize,
    /// Direct query signals for this record; `episode` means context only.
    pub found_by: Vec<String>,
    /// Whether this record itself, rather than its neighbourhood, backs the query.
    pub query_backed: bool,
    /// Relevance to the query, from the lexical signal and graph distance.
    pub relevance: f64,
}

impl ExperienceRecord {
    fn to_json(&self) -> Value {
        json!({
            "id": self.node_id,
            "space": self.space_id,
            "revision": self.revision,
            "kind": self.kind.to_string(),
            "label": self.label,
            "excerpt": self.excerpt,
            "source": self.source,
            "date": self.date,
            "date_confidence": self.date_confidence.as_str(),
            "label_repeats": self.label_repeats,
            "terms_matched": self.terms_matched,
            "valid_from": self.valid_from,
            "valid_to": self.valid_to,
            "recorded_at": self.recorded_at,
            "superseded_at": self.superseded_at,
            "source_agent": self.agent_id,
            "source_session": self.session_id,
            "found_by": self.found_by,
            "query_backed": self.query_backed,
            "relevance": self.relevance,
        })
    }
}

/// One stretch of connected past work.
#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    /// Identifier of the connecting session node, or of the strongest member.
    pub id: String,
    /// Label of the connecting session node when one exists.
    pub title: String,
    /// Whether a stored session node tied these records together.
    pub anchored: bool,
    /// Member records ordered by date.
    pub records: Vec<ExperienceRecord>,
    /// Earliest and latest member dates.
    pub span: (Timestamp, Timestamp),
    /// Summed relevance of the members.
    pub score: f64,
}

impl Episode {
    /// Counts members of one stored category.
    #[must_use]
    pub fn count_of(&self, kind: NodeKind) -> usize {
        self.records
            .iter()
            .filter(|record| record.kind == kind)
            .count()
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "anchored": self.anchored,
            "from": iso_date(self.span.0),
            "to": iso_date(self.span.1),
            "score": self.score,
            "counts": {
                "decision": self.count_of(NodeKind::Decision),
                "error": self.count_of(NodeKind::Error),
                "memory": self.count_of(NodeKind::Memory),
            },
            "records": self.records.iter().map(ExperienceRecord::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Whether the retrieved experience justifies a template at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceVerdict {
    /// Enough independent episodes to describe a repeatable pattern.
    Sufficient,
    /// A real pattern is visible but rests on too little to generalize.
    Thin(String),
    /// Nothing worth calling a template; the reason names what is missing.
    Insufficient(String),
}

impl EvidenceVerdict {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Sufficient => "sufficient",
            Self::Thin(_) => "thin",
            Self::Insufficient(_) => "insufficient",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Sufficient => None,
            Self::Thin(reason) | Self::Insufficient(reason) => Some(reason),
        }
    }
}

/// The part a step played in past work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepRole {
    /// A recorded decision.
    Decision,
    /// A recorded error or gotcha.
    Pitfall,
    /// Supporting context recorded as a plain memory.
    Context,
}

impl StepRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Pitfall => "pitfall",
            Self::Context => "context",
        }
    }

    const fn of(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Decision => Self::Decision,
            NodeKind::Error => Self::Pitfall,
            _ => Self::Context,
        }
    }
}

/// What is factually known about a step after it was recorded.
///
/// None of these are success judgements. The engine cannot know whether a step
/// worked; it knows what the record still asserts and what followed it in time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    /// The record is still valid and was never closed.
    StillStated,
    /// The record's validity window was closed at this world time.
    Closed(Timestamp),
    /// Errors were recorded later in the same episode; identifiers are the errors.
    FollowedByError(Vec<String>),
    /// Dates are import artifacts, so nothing can be said about what followed.
    Unknown,
}

impl StepOutcome {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::StillStated => "still-stated",
            Self::Closed(_) => "closed",
            Self::FollowedByError(_) => "followed-by-error",
            Self::Unknown => "unknown",
        }
    }
}

/// One template item, always tied to the records that produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateStep {
    /// One-based position in the reconstructed order.
    pub position: usize,
    /// Verbatim quote of a stored record; never generated text.
    pub statement: String,
    /// The part this step played.
    pub role: StepRole,
    /// Records backing the step; never empty.
    pub sources: Vec<ExperienceRecord>,
    /// Distinct episodes in which this step appeared.
    pub episodes: usize,
    /// What is factually known about the step after it was recorded.
    pub outcome: StepOutcome,
}

impl TemplateStep {
    /// Whether any source carries the words that were asked about.
    ///
    /// A step reached only by belonging to a relevant episode is still real past
    /// work, but it answers a different question than the one the caller typed,
    /// and the report has to keep the two apart.
    #[must_use]
    pub fn matches_query(&self) -> bool {
        self.sources.iter().any(|source| source.query_backed)
    }

    /// Whether separate records from separate episodes support the step.
    ///
    /// Both conditions are required. Episodes overlap when a record belongs to
    /// several sessions, so episode count alone would let one record vouch for
    /// itself and read as a proven pattern.
    #[must_use]
    pub fn is_repeated(&self) -> bool {
        self.episodes > 1 && self.sources.len() > 1
    }

    fn to_json(&self) -> Value {
        json!({
            "position": self.position,
            "statement": self.statement,
            "role": self.role.as_str(),
            "episodes": self.episodes,
            "repeated": self.is_repeated(),
            "support": if self.is_repeated() { "repeated" } else { "single-episode" },
            "found_by": if self.matches_query() { "query" } else { "episode" },
            "outcome": self.outcome.as_str(),
            "outcome_detail": match &self.outcome {
                StepOutcome::Closed(at) => json!(iso_date(*at)),
                StepOutcome::FollowedByError(ids) => json!(ids),
                _ => Value::Null,
            },
            "sources": self.sources.iter().map(ExperienceRecord::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Result of `brain similar`: past work that resembles the query.
#[derive(Clone, Debug, PartialEq)]
pub struct SimilarReport {
    /// Namespace searched.
    pub space_id: String,
    /// Bi-temporal coordinate at which the report was assembled.
    pub as_of: AsOf,
    /// The query as asked.
    pub query: String,
    /// Strongest stretches of past work, capped at `max_episodes`.
    pub episodes: Vec<Episode>,
    /// Episodes assembled in total, before that cap.
    pub episodes_total: usize,
    /// Records retrieved before episode assembly.
    pub matched_records: usize,
    /// Whether this much experience justifies a template.
    pub verdict: EvidenceVerdict,
    /// Honest degradation notices.
    pub warnings: Vec<String>,
}

impl SimilarReport {
    /// Renders the report for a CLI or MCP caller.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "status": match &self.verdict {
                EvidenceVerdict::Sufficient => "ok",
                EvidenceVerdict::Insufficient(_) if self.episodes.is_empty() => "empty",
                EvidenceVerdict::Thin(_) | EvidenceVerdict::Insufficient(_) => "insufficient_evidence",
            },
            "space": self.space_id,
            "as_of": {
                "valid_at": self.as_of.valid_at,
                "recorded_at": self.as_of.recorded_at,
            },
            "query": self.query,
            "matched_records": self.matched_records,
            "episodes_total": self.episodes_total,
            "episodes_shown": self.episodes.len(),
            "verdict": self.verdict.as_str(),
            "verdict_reason": self.verdict.reason(),
            "episodes": self.episodes.iter().map(Episode::to_json).collect::<Vec<_>>(),
            "warnings": self.warnings,
        })
    }
}

/// Result of `brain template`: a plan assembled from past work.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateReport {
    /// Namespace searched.
    pub space_id: String,
    /// Bi-temporal coordinate at which the report was assembled.
    pub as_of: AsOf,
    /// The query as asked.
    pub query: String,
    /// Reconstructed steps in the order the records suggest.
    pub steps: Vec<TemplateStep>,
    /// Recorded errors from the same episodes, kept separate from the steps.
    pub pitfalls: Vec<TemplateStep>,
    /// Whether this much experience justifies a template.
    pub verdict: EvidenceVerdict,
    /// Identifiers of the episodes the template was drawn from.
    pub episodes_used: Vec<String>,
    /// Basis of the step ordering, since stored dates are not always trustworthy.
    pub order_basis: &'static str,
    /// Honest degradation notices.
    pub warnings: Vec<String>,
}

impl TemplateReport {
    /// Renders the report for a CLI or MCP caller.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": REPORT_SCHEMA_VERSION,
            "status": if matches!(&self.verdict, EvidenceVerdict::Sufficient) && !self.steps.is_empty() {
                "ok"
            } else {
                "refused"
            },
            "space": self.space_id,
            "as_of": {
                "valid_at": self.as_of.valid_at,
                "recorded_at": self.as_of.recorded_at,
            },
            "query": self.query,
            "verdict": self.verdict.as_str(),
            "verdict_reason": self.verdict.reason(),
            "order_basis": self.order_basis,
            "episodes_used": self.episodes_used,
            "steps": self.steps.iter().map(TemplateStep::to_json).collect::<Vec<_>>(),
            "pitfalls": self.pitfalls.iter().map(TemplateStep::to_json).collect::<Vec<_>>(),
            "warnings": self.warnings,
        })
    }
}

/// Finds past work resembling `query` by text and by graph structure.
///
/// # Errors
///
/// Returns an error for an empty query or space identifier, or when the database
/// cannot be read.
pub fn similar(
    connection: &Connection,
    space_id: &str,
    query: &str,
    options: &ExperienceOptions,
) -> Result<SimilarReport, TemplateError> {
    let context = collect(connection, space_id, query, options)?;
    let verdict = verdict_for(&context, options);
    let episodes_total = context.episodes.len();
    let mut episodes = context.episodes;
    episodes.truncate(options.max_episodes);
    Ok(SimilarReport {
        space_id: space_id.to_owned(),
        as_of: options.as_of,
        query: query.to_owned(),
        matched_records: context.records.len(),
        verdict,
        episodes,
        episodes_total,
        warnings: context.warnings,
    })
}

/// Assembles an applicable template from the work `similar` would return.
///
/// Steps are merged across episodes so that repetition becomes visible: a step
/// seen in three episodes is a pattern, a step seen once is one person's Tuesday.
///
/// # Errors
///
/// Returns an error for an empty query or space identifier, or when the database
/// cannot be read.
pub fn template(
    connection: &Connection,
    space_id: &str,
    query: &str,
    options: &ExperienceOptions,
) -> Result<TemplateReport, TemplateError> {
    let context = collect(connection, space_id, query, options)?;
    let verdict = verdict_for(&context, options);
    let mut warnings = context.warnings;
    let anchored_episodes = context
        .episodes
        .iter()
        .filter(|episode| episode.anchored)
        .cloned()
        .collect::<Vec<_>>();
    let episodes_used = anchored_episodes
        .iter()
        .map(|episode| episode.id.clone())
        .collect::<Vec<_>>();

    if let Some(reason) = verdict.reason() {
        warnings.push(format!(
            "no template produced: {reason}. The records found are listed by `brain similar`."
        ));
        return Ok(TemplateReport {
            space_id: space_id.to_owned(),
            as_of: options.as_of,
            query: query.to_owned(),
            steps: Vec::new(),
            pitfalls: Vec::new(),
            verdict,
            episodes_used: Vec::new(),
            order_basis: "none",
            warnings,
        });
    }

    let (mut steps, pitfalls) = build_steps(&anchored_episodes, options.as_of.valid_at);
    let order_basis = if steps
        .iter()
        .flat_map(|step| &step.sources)
        .all(|source| source.date_confidence == DateConfidence::BulkImport)
    {
        warnings.push(
            "every source shares an import timestamp; step order reflects relevance, not chronology"
                .to_owned(),
        );
        "query match, episode support, then relevance (valid_from values are import artifacts)"
    } else {
        "query match, independent episode support, then valid_from and statement"
    };
    for (index, step) in steps.iter_mut().enumerate() {
        step.position = index + 1;
    }

    Ok(TemplateReport {
        space_id: space_id.to_owned(),
        as_of: options.as_of,
        query: query.to_owned(),
        steps,
        pitfalls,
        verdict,
        episodes_used,
        order_basis,
        warnings,
    })
}

/// Everything both commands need, gathered once.
struct Context {
    records: Vec<ExperienceRecord>,
    episodes: Vec<Episode>,
    warnings: Vec<String>,
}

fn collect(
    connection: &Connection,
    space_id: &str,
    query: &str,
    options: &ExperienceOptions,
) -> Result<Context, TemplateError> {
    if space_id.trim().is_empty() {
        return Err(TemplateError::InvalidQuery(
            "space_id cannot be empty".to_owned(),
        ));
    }
    if query.trim().is_empty() {
        return Err(TemplateError::InvalidQuery(
            "query text cannot be empty".to_owned(),
        ));
    }

    let mut warnings = options.degradation_warnings.clone();
    let bulk = bulk_import_timestamps(
        connection,
        space_id,
        options.as_of,
        options.bulk_import_threshold,
    )?;
    let boilerplate = boilerplate_labels(
        connection,
        space_id,
        options.as_of,
        options.boilerplate_label_threshold,
    )?;
    let terms = query_terms(query);
    let match_expression = fts_expression(&terms);

    let mut seeds: BTreeMap<String, f64> = BTreeMap::new();
    let mut seed_sources: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut trusted_ids = HashSet::new();
    if match_expression.is_empty() {
        warnings.push(
            "the query has no searchable terms; only caller-supplied seeds were used".to_owned(),
        );
    } else {
        let (lexical_ids, lexical_truncated) = lexical_seed_ids(
            connection,
            space_id,
            options.as_of,
            &match_expression,
            options.max_seeds,
        )?;
        if lexical_truncated {
            warnings.push(format!(
                "full-text candidates were bounded to {}; this experience lookup is not exhaustive",
                options.max_seeds
            ));
        }
        for (index, node_id) in lexical_ids.into_iter().enumerate() {
            let relevance = 1.0 / (1.0 + index as f64);
            seeds
                .entry(node_id.clone())
                .and_modify(|score| *score = score.max(relevance))
                .or_insert(relevance);
            seed_sources
                .entry(node_id)
                .or_default()
                .insert("bm25".to_owned());
        }
    }
    for seed in &options.trusted_seeds {
        if seed.node_id.trim().is_empty() || !seed.relevance.is_finite() || seed.relevance <= 0.0 {
            warnings.push(format!(
                "ignored invalid {} seed {:?} with relevance {}",
                seed.signal.as_str(),
                seed.node_id,
                seed.relevance
            ));
            continue;
        }
        let relevance = seed.relevance.clamp(f64::MIN_POSITIVE, 1.0);
        seeds
            .entry(seed.node_id.clone())
            .and_modify(|score| *score = score.max(relevance))
            .or_insert(relevance);
        seed_sources
            .entry(seed.node_id.clone())
            .or_default()
            .insert(seed.signal.as_str().to_owned());
        trusted_ids.insert(seed.node_id.clone());
    }
    if trusted_ids.is_empty() {
        warnings.push(
            "semantic seeds were not supplied; matching used full-text and graph structure only"
                .to_owned(),
        );
    }
    if seeds.is_empty() {
        return Ok(Context {
            records: Vec::new(),
            episodes: Vec::new(),
            warnings,
        });
    }

    // Weigh the candidates against the query before letting them pull in a
    // neighbourhood. A record that shares one word out of four is not experience
    // about this task, and expanding from it would dress that coincidence up as
    // a whole episode of relevant past work.
    let candidates = load_nodes(
        connection,
        space_id,
        options.as_of,
        &seeds.keys().cloned().collect::<Vec<_>>(),
        &bulk,
        &boilerplate,
        &terms,
    )?;
    let required = required_terms(terms.len());
    let mut found_terms = HashSet::new();
    for record in &candidates {
        let haystack = query_terms(&format!("{} {}", record.label, record.excerpt))
            .into_iter()
            .collect::<HashSet<_>>();
        found_terms.extend(
            terms
                .iter()
                .filter(|term| haystack.contains(term.as_str()))
                .cloned(),
        );
    }
    seeds.retain(|node_id, _| {
        trusted_ids.contains(node_id)
            || candidates
                .iter()
                .any(|record| &record.node_id == node_id && record.terms_matched >= required)
    });
    seed_sources.retain(|node_id, _| seeds.contains_key(node_id));
    if seeds.is_empty() {
        let missing = terms
            .iter()
            .filter(|term| !found_terms.contains(*term))
            .cloned()
            .collect::<Vec<_>>();
        warnings.push(if missing.is_empty() {
            format!(
                "no stored record covers at least {required} of the {} terms in this description",
                terms.len()
            )
        } else {
            format!(
                "nothing in this archive mentions {}",
                missing
                    .iter()
                    .map(|term| format!("{term:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        return Ok(Context {
            records: Vec::new(),
            episodes: Vec::new(),
            warnings,
        });
    }

    let seed_ids = seeds.keys().cloned().collect::<Vec<_>>();
    let mut links = incident_edges(connection, space_id, options.as_of, &seed_ids)?;
    let mut wanted = seeds.keys().cloned().collect::<HashSet<_>>();
    let mut fanout_truncated = reach_out(&links, &seeds.keys().cloned().collect(), &mut wanted);

    let mut loaded = load_nodes(
        connection,
        space_id,
        options.as_of,
        &wanted.iter().cloned().collect::<Vec<_>>(),
        &bulk,
        &boilerplate,
        &terms,
    )?;

    // A session node reached from a seed names the rest of that stretch of work.
    // Without this second hop an episode holds only the fragments that happened
    // to match the wording of the query, which is a re-ranked search result
    // rather than the past work the caller asked to see.
    let connector_ids = loaded
        .iter()
        .filter(|node| node.kind == NodeKind::Session)
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    if !connector_ids.is_empty() {
        let member_links = incident_edges(connection, space_id, options.as_of, &connector_ids)?;
        let mut members = HashSet::new();
        fanout_truncated |= reach_out(
            &member_links,
            &connector_ids.iter().cloned().collect(),
            &mut members,
        );
        let extra = members
            .into_iter()
            .filter(|id| !wanted.contains(id))
            .collect::<Vec<_>>();
        if !extra.is_empty() {
            loaded.extend(load_nodes(
                connection,
                space_id,
                options.as_of,
                &extra,
                &bulk,
                &boilerplate,
                &terms,
            )?);
            wanted.extend(extra);
        }
        links.extend(member_links);
    }
    if fanout_truncated {
        warnings.push(format!(
            "episode expansion was bounded to {MAX_NEIGHBOUR_FANOUT} neighbours per seed or session; this experience lookup is not exhaustive"
        ));
    }

    let connectors = loaded
        .iter()
        .filter(|node| node.kind == NodeKind::Session)
        .map(|node| (node.node_id.clone(), node.label.clone()))
        .collect::<HashMap<_, _>>();

    let reachability = spread_relevance(&links, &seeds);
    let mut records = loaded
        .into_iter()
        .filter(|node| EXPERIENCE_KINDS.contains(&node.kind))
        .map(|mut node| {
            node.relevance = reachability.get(&node.node_id).copied().unwrap_or(0.0);
            if let Some(sources) = seed_sources.get(&node.node_id) {
                node.found_by = sources.iter().cloned().collect();
                node.query_backed = true;
            } else {
                node.found_by = vec!["episode".to_owned()];
                node.query_backed = false;
            }
            node
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        right
            .relevance
            .total_cmp(&left.relevance)
            .then_with(|| left.node_id.cmp(&right.node_id))
    });

    if records
        .iter()
        .any(|record| record.date_confidence == DateConfidence::BulkImport)
    {
        warnings.push(
            "some records share one import timestamp; their dates describe the import, not the work"
                .to_owned(),
        );
    }
    let structural = records
        .iter()
        .filter(|record| record.label_repeats.is_some())
        .count();
    if structural > 0 {
        let worst = records
            .iter()
            .filter_map(|record| {
                record
                    .label_repeats
                    .map(|repeats| (repeats, record.label.as_str()))
            })
            .max();
        if let Some((repeats, label)) = worst {
            warnings.push(format!(
                "{structural} of {} matched records are titled with a section header reused across \
                 the archive (worst: {label:?} on {repeats} nodes); they are quoted as context but \
                 never used as template steps",
                records.len()
            ));
        }
    }

    // Episodes stay complete here. `similar` shows the strongest few, but a
    // template that only ever saw eight episodes could not tell a repeated step
    // from a lucky one.
    let mut episodes = assemble_episodes(&records, &links, &connectors);
    episodes.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(Context {
        records,
        episodes,
        warnings,
    })
}

/// Adds the neighbours of `from` to `into`, capped so one hub cannot flood the set.
fn reach_out(links: &[Link], from: &HashSet<String>, into: &mut HashSet<String>) -> bool {
    into.extend(from.iter().cloned());
    let mut fanout: HashMap<&str, usize> = HashMap::new();
    let mut truncated = false;
    for link in links {
        for (near, far) in [(&link.from, &link.to), (&link.to, &link.from)] {
            if !from.contains(near.as_str()) || into.contains(far.as_str()) {
                continue;
            }
            let used = fanout.entry(near.as_str()).or_default();
            if *used >= MAX_NEIGHBOUR_FANOUT {
                truncated = true;
                continue;
            }
            *used += 1;
            into.insert(far.clone());
        }
    }
    truncated
}

/// Spreads seed relevance outward, one graph hop per round.
///
/// Two rounds, because a record reached through its session node is two hops
/// from the query: seed → session → record. Each hop keeps `NEIGHBOUR_DECAY` of
/// the relevance, so evidence found by wording always outranks evidence found by
/// association.
fn spread_relevance(links: &[Link], seeds: &BTreeMap<String, f64>) -> HashMap<String, f64> {
    let mut scored = seeds
        .iter()
        .map(|(node_id, score)| (node_id.clone(), *score))
        .collect::<HashMap<_, _>>();
    for _ in 0..2 {
        let mut next = scored.clone();
        for link in links {
            for (near, far) in [(&link.from, &link.to), (&link.to, &link.from)] {
                let Some(&source) = scored.get(near.as_str()) else {
                    continue;
                };
                let candidate = NEIGHBOUR_DECAY * source * link.weight;
                let entry = next.entry(far.clone()).or_insert(0.0);
                if candidate > *entry {
                    *entry = candidate;
                }
            }
        }
        scored = next;
    }
    // A seed's own relevance is what the query said; association cannot raise it.
    for (node_id, score) in seeds {
        scored.insert(node_id.clone(), *score);
    }
    scored
}

/// One graph edge between two logical nodes.
struct Link {
    from: String,
    to: String,
    weight: f64,
}

fn verdict_for(context: &Context, options: &ExperienceOptions) -> EvidenceVerdict {
    let anchored_record_ids = context
        .episodes
        .iter()
        .filter(|episode| episode.anchored)
        .flat_map(|episode| &episode.records)
        .map(|record| record.node_id.as_str())
        .collect::<HashSet<_>>();
    let eligible_ids = context
        .records
        .iter()
        .filter(|record| {
            record.query_backed
                && record.label_repeats.is_none()
                && anchored_record_ids.contains(record.node_id.as_str())
        })
        .map(|record| record.node_id.as_str())
        .collect::<HashSet<_>>();
    let records = eligible_ids.len();

    // A session is independent evidence only when it contributes a qualifying
    // record no earlier session already claimed. One record attached to two
    // sessions must never count twice.
    let mut seen_records = HashSet::new();
    let mut episodes = 0usize;
    for episode in &context.episodes {
        if !episode.anchored {
            continue;
        }
        let contributed = episode
            .records
            .iter()
            .filter(|record| eligible_ids.contains(record.node_id.as_str()))
            .map(|record| record.node_id.as_str())
            .collect::<Vec<_>>();
        if contributed
            .iter()
            .any(|node_id| !seen_records.contains(node_id))
        {
            episodes += 1;
            seen_records.extend(contributed);
        }
    }

    if records == 0 {
        return EvidenceVerdict::Insufficient(
            "no active, non-structural record in an anchored episode directly backs this description"
                .to_owned(),
        );
    }
    let min_episodes = options.min_episodes.max(1);
    let min_records = options.min_records.max(1);
    if episodes < min_episodes || records < min_records {
        return EvidenceVerdict::Insufficient(format!(
            "found {records} query-backed record(s) in {episodes} independent anchored episode(s); \
             a template needs at least {min_records} records across {min_episodes} such episodes"
        ));
    }
    if episodes < THIN_EPISODES || records < THIN_RECORDS {
        return EvidenceVerdict::Thin(format!(
            "only {records} query-backed records across {episodes} independent episodes; \
             this describes individual cases but not a reusable pattern"
        ));
    }

    let (steps, _) = build_steps(&context.episodes, options.as_of.valid_at);
    let repeated_query_step = steps.iter().any(|step| {
        let source_ids = step
            .sources
            .iter()
            .map(|source| source.node_id.as_str())
            .filter(|node_id| eligible_ids.contains(node_id))
            .collect::<HashSet<_>>();
        if source_ids.len() < 2 {
            return false;
        }
        context
            .episodes
            .iter()
            .filter(|episode| {
                episode.anchored
                    && episode
                        .records
                        .iter()
                        .any(|record| source_ids.contains(record.node_id.as_str()))
            })
            .take(2)
            .count()
            >= 2
    });
    if !repeated_query_step {
        return EvidenceVerdict::Thin(
            "no reusable step is independently repeated by at least two query-backed records"
                .to_owned(),
        );
    }
    EvidenceVerdict::Sufficient
}

/// Groups records into episodes, one per stored session node.
///
/// Connectivity is deliberately not followed transitively. Two earlier designs
/// fail on a real graph: the `session_id` column is a single value after a bulk
/// import, and a union-find over edges merges unrelated work into one blob,
/// because `relates_to` chains reach everywhere. Measured on a 10590-node
/// database, transitive grouping put 91 of 94 matched records into one
/// "episode" spanning six weeks. That both invents a shared history and hides
/// repetition, since a step repeated inside one blob still counts as one
/// episode.
///
/// So an episode is exactly what storage can vouch for: a session node and the
/// records directly attached to it. A record attached to several sessions
/// appears in several episodes, which is true rather than convenient. A record
/// with no session stands alone; claiming it shares a stretch of work with
/// anything else would be a guess.
fn assemble_episodes(
    records: &[ExperienceRecord],
    links: &[Link],
    connectors: &HashMap<String, String>,
) -> Vec<Episode> {
    let by_id = records
        .iter()
        .map(|record| (record.node_id.as_str(), record))
        .collect::<HashMap<_, _>>();

    let mut membership: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for link in links {
        for (connector, member) in [(&link.from, &link.to), (&link.to, &link.from)] {
            if connectors.contains_key(connector.as_str()) && by_id.contains_key(member.as_str()) {
                membership
                    .entry(connector.as_str())
                    .or_default()
                    .insert(member.as_str());
            }
        }
    }

    let anchored_ids = membership
        .values()
        .flatten()
        .copied()
        .collect::<HashSet<_>>();

    let mut episodes = membership
        .iter()
        .map(|(connector, member_ids)| {
            let members = member_ids
                .iter()
                .filter_map(|id| by_id.get(id).map(|record| (*record).clone()))
                .collect::<Vec<_>>();
            episode_from(
                (*connector).to_owned(),
                connectors
                    .get(*connector)
                    .cloned()
                    .unwrap_or_else(|| (*connector).to_owned()),
                true,
                members,
            )
        })
        .collect::<Vec<_>>();

    // A record no session vouches for is its own episode, not a guessed member
    // of somebody else's.
    episodes.extend(
        records
            .iter()
            .filter(|record| !anchored_ids.contains(record.node_id.as_str()))
            .map(|record| {
                episode_from(
                    record.node_id.clone(),
                    record.label.clone(),
                    false,
                    vec![record.clone()],
                )
            }),
    );
    episodes
}

/// Builds one episode from its members, ordering them by date.
fn episode_from(
    id: String,
    title: String,
    anchored: bool,
    members: Vec<ExperienceRecord>,
) -> Episode {
    let mut members = members;
    members.sort_by(|left, right| {
        left.valid_from
            .cmp(&right.valid_from)
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let score = members.iter().map(|record| record.relevance).sum();
    let span = (
        members.first().map_or(0, |record| record.valid_from),
        members.last().map_or(0, |record| record.valid_from),
    );
    Episode {
        id,
        title,
        anchored,
        records: members,
        span,
        score,
    }
}

/// Merges per-episode records into steps and pitfalls.
///
/// A statement is always a stored label. Two labels that overlap strongly are the
/// same step seen twice, which is the only evidence of a repeatable pattern this
/// module accepts.
/// Whether a label is a filesystem path and nothing else.
///
/// A record titled `/example-home/someone/projects/thing` says where work happened, not
/// what was done. Quoting it as a step tells the reader nothing and publishes
/// somebody's directory layout. A label that merely mentions a path further in
/// — "Fixed the parser in src/lexer.rs" — is a real statement and stays.
fn label_is_only_a_path(label: &str) -> bool {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.split_whitespace().count() > 1 {
        return false;
    }
    trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
        || (trimmed.contains('/') && trimmed.matches('/').count() >= 2)
}

fn build_steps(episodes: &[Episode], now: Timestamp) -> (Vec<TemplateStep>, Vec<TemplateStep>) {
    let mut merged: Vec<Merge> = Vec::new();

    for episode in episodes {
        let later_errors = episode
            .records
            .iter()
            .filter(|record| record.kind == NodeKind::Error)
            .collect::<Vec<_>>();
        for record in &episode.records {
            // A heading that titles dozens of unrelated nodes says nothing about
            // this task, and merging on it would fabricate the strongest step in
            // the template out of pure formatting.
            if record.label_repeats.is_some() {
                continue;
            }
            // A label that is only a path names a location, not something that
            // was done, so it cannot be a step — and it drags one machine's
            // directory layout into a report meant to be read by anyone.
            if label_is_only_a_path(&record.label) {
                continue;
            }
            let tokens = significant_tokens(&record.label);
            let role = StepRole::of(record.kind);
            let existing = merged.iter_mut().find(|merge| {
                merge.step.role == role && dice(&merge.tokens, &tokens) >= STEP_MERGE_SIMILARITY
            });
            match existing {
                Some(merge) => {
                    merge.tokens.extend(tokens);
                    merge.episodes.insert(episode.id.clone());
                    // One record attached to two sessions is still one record.
                    // Quoting it twice would manufacture the very repetition
                    // this module exists to measure.
                    if merge.records.insert(record.node_id.clone()) {
                        merge.step.sources.push(record.clone());
                    }
                    merge.step.episodes = merge.episodes.len();
                }
                None => {
                    let outcome = outcome_for(record, &later_errors, now);
                    merged.push(Merge {
                        tokens,
                        step: TemplateStep {
                            position: 0,
                            statement: record.label.clone(),
                            role,
                            sources: vec![record.clone()],
                            episodes: 1,
                            outcome,
                        },
                        episodes: HashSet::from([episode.id.clone()]),
                        records: HashSet::from([record.node_id.clone()]),
                    });
                }
            }
        }
    }

    let (pitfalls, mut steps): (Vec<_>, Vec<_>) = merged
        .into_iter()
        .map(|merge| merge.step)
        .partition(|step| step.role == StepRole::Pitfall);

    steps.sort_by(|left, right| {
        // What the caller asked about comes before what merely shared a session.
        right
            .matches_query()
            .cmp(&left.matches_query())
            .then_with(|| right.episodes.cmp(&left.episodes))
            .then_with(|| earliest(left).cmp(&earliest(right)))
            .then_with(|| left.statement.cmp(&right.statement))
    });
    let mut pitfalls = pitfalls;
    pitfalls.sort_by(|left, right| {
        // What the caller asked about comes before what merely shared a session.
        right
            .matches_query()
            .cmp(&left.matches_query())
            .then_with(|| right.episodes.cmp(&left.episodes))
            .then_with(|| earliest(left).cmp(&earliest(right)))
            .then_with(|| left.statement.cmp(&right.statement))
    });
    for (index, pitfall) in pitfalls.iter_mut().enumerate() {
        pitfall.position = index + 1;
    }
    (steps, pitfalls)
}

/// One step under construction, with the identities needed to avoid
/// double-counting a record seen through two sessions.
struct Merge {
    tokens: HashSet<String>,
    step: TemplateStep,
    episodes: HashSet<String>,
    records: HashSet<String>,
}

fn earliest(step: &TemplateStep) -> Timestamp {
    step.sources
        .iter()
        .map(|source| source.valid_from)
        .min()
        .unwrap_or(0)
}

/// States what is known after a record, never whether it succeeded.
fn outcome_for(
    record: &ExperienceRecord,
    later_errors: &[&ExperienceRecord],
    now: Timestamp,
) -> StepOutcome {
    if let Some(valid_to) = record.valid_to.filter(|valid_to| *valid_to <= now) {
        return StepOutcome::Closed(valid_to);
    }
    if record.kind == NodeKind::Error {
        return StepOutcome::StillStated;
    }
    if record.date_confidence == DateConfidence::BulkImport {
        return StepOutcome::Unknown;
    }
    let following = later_errors
        .iter()
        .filter(|error| {
            error.valid_from > record.valid_from
                && error.date_confidence == DateConfidence::Observed
        })
        .map(|error| error.node_id.clone())
        .collect::<Vec<_>>();
    if following.is_empty() {
        StepOutcome::StillStated
    } else {
        StepOutcome::FollowedByError(following)
    }
}

fn lexical_seed_ids(
    connection: &Connection,
    space_id: &str,
    as_of: AsOf,
    match_expression: &str,
    limit: usize,
) -> Result<(Vec<String>, bool), TemplateError> {
    let kinds = kind_placeholders(6);
    let sql = format!(
        "SELECT nv.node_id
         FROM node_versions_fts
         JOIN node_versions nv ON nv.row_id = node_versions_fts.rowid
         WHERE node_versions_fts MATCH ?1
           AND nv.space_id = ?2
           AND nv.valid_from <= ?3
           AND (nv.valid_to IS NULL OR ?3 < nv.valid_to)
           AND nv.recorded_at <= ?4
           AND (nv.superseded_at IS NULL OR ?4 < nv.superseded_at)
           AND nv.kind IN ({kinds})
         ORDER BY bm25(node_versions_fts)
         LIMIT ?5"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut arguments: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(match_expression.to_owned()),
        Box::new(space_id.to_owned()),
        Box::new(as_of.valid_at),
        Box::new(as_of.recorded_at),
        Box::new(i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX)),
    ];
    arguments.extend(
        EXPERIENCE_KINDS
            .iter()
            .map(|kind| Box::new(kind.to_string()) as Box<dyn rusqlite::ToSql>),
    );
    let rows = statement.query_map(params_from_iter(arguments.iter()), |row| {
        row.get::<_, String>(0)
    })?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    let truncated = ids.len() > limit;
    ids.truncate(limit);
    Ok((ids, truncated))
}

fn incident_edges(
    connection: &Connection,
    space_id: &str,
    as_of: AsOf,
    node_ids: &[String],
) -> Result<Vec<Link>, TemplateError> {
    if node_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = id_placeholders(4, node_ids.len());
    let second = id_placeholders(4 + node_ids.len(), node_ids.len());
    let sql = format!(
        "SELECT e.from_node_id, e.to_node_id, ev.weight
         FROM edges e
         JOIN edge_versions ev ON ev.edge_id = e.id AND ev.space_id = e.space_id
         WHERE e.space_id = ?1
           AND ev.valid_from <= ?2
           AND (ev.valid_to IS NULL OR ?2 < ev.valid_to)
           AND ev.recorded_at <= ?3
           AND (ev.superseded_at IS NULL OR ?3 < ev.superseded_at)
           AND ev.weight > 0.0
           AND (e.from_node_id IN ({placeholders}) OR e.to_node_id IN ({second}))
         ORDER BY ev.weight DESC, e.id ASC"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut arguments: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(space_id.to_owned()),
        Box::new(as_of.valid_at),
        Box::new(as_of.recorded_at),
    ];
    for _ in 0..2 {
        arguments.extend(
            node_ids
                .iter()
                .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>),
        );
    }
    let rows = statement.query_map(params_from_iter(arguments.iter()), |row| {
        Ok(Link {
            from: row.get(0)?,
            to: row.get(1)?,
            weight: row.get::<_, f64>(2)?,
        })
    })?;
    let mut links = Vec::new();
    for row in rows {
        links.push(row?);
    }
    Ok(links)
}

fn load_nodes(
    connection: &Connection,
    space_id: &str,
    as_of: AsOf,
    node_ids: &[String],
    bulk: &HashSet<Timestamp>,
    boilerplate: &HashMap<String, usize>,
    terms: &[String],
) -> Result<Vec<ExperienceRecord>, TemplateError> {
    if node_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = id_placeholders(4, node_ids.len());
    let sql = format!(
        "SELECT nv.node_id, nv.space_id, nv.revision, nv.kind, nv.label, nv.body,
                nv.source, nv.valid_from, nv.valid_to, nv.recorded_at, nv.superseded_at,
                nv.agent_id, nv.session_id
         FROM node_versions nv
         WHERE nv.space_id = ?1
           AND nv.valid_from <= ?2
           AND (nv.valid_to IS NULL OR ?2 < nv.valid_to)
           AND nv.recorded_at <= ?3
           AND (nv.superseded_at IS NULL OR ?3 < nv.superseded_at)
           AND nv.node_id IN ({placeholders})"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut arguments: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(space_id.to_owned()),
        Box::new(as_of.valid_at),
        Box::new(as_of.recorded_at),
    ];
    arguments.extend(
        node_ids
            .iter()
            .map(|id| Box::new(id.clone()) as Box<dyn rusqlite::ToSql>),
    );
    let rows = statement.query_map(params_from_iter(arguments.iter()), |row| {
        let stored: String = row.get(3)?;
        let valid_from: Timestamp = row.get(7)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            stored,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            valid_from,
            row.get::<_, Option<Timestamp>>(8)?,
            row.get::<_, Timestamp>(9)?,
            row.get::<_, Option<Timestamp>>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;

    let mut records = Vec::new();
    for row in rows {
        let (
            node_id,
            stored_space,
            revision,
            stored_kind,
            label,
            body,
            source,
            valid_from,
            valid_to,
            recorded_at,
            superseded_at,
            agent_id,
            session_id,
        ) = row?;
        let Some(kind) = NodeKind::from_stored(&stored_kind) else {
            continue;
        };
        let label_repeats = boilerplate.get(&label).copied();
        let terms_matched = terms_present(terms, &label, &body);
        records.push(ExperienceRecord {
            node_id,
            space_id: stored_space,
            revision,
            kind,
            label,
            excerpt: truncate_chars(&body, EXCERPT_CHARS),
            source,
            valid_from,
            valid_to,
            recorded_at,
            superseded_at,
            agent_id,
            session_id,
            date: iso_date(valid_from),
            date_confidence: if bulk.contains(&valid_from) {
                DateConfidence::BulkImport
            } else {
                DateConfidence::Observed
            },
            label_repeats,
            terms_matched,
            found_by: Vec::new(),
            query_backed: false,
            relevance: 0.0,
        });
    }
    Ok(records)
}

/// Finds labels shared by so many nodes that they title a section, not a lesson.
///
/// Only the offending labels come back, so the result stays a handful of rows
/// however large the archive is.
fn boilerplate_labels(
    connection: &Connection,
    space_id: &str,
    as_of: AsOf,
    threshold: usize,
) -> Result<HashMap<String, usize>, TemplateError> {
    let kinds = kind_placeholders(5);
    let sql = format!(
        "SELECT label, COUNT(*)
         FROM node_versions
         WHERE space_id = ?1
           AND valid_from <= ?2
           AND (valid_to IS NULL OR ?2 < valid_to)
           AND recorded_at <= ?3
           AND (superseded_at IS NULL OR ?3 < superseded_at)
           AND kind IN ({kinds})
         GROUP BY label
         HAVING COUNT(*) > ?4"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut arguments: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(space_id.to_owned()),
        Box::new(as_of.valid_at),
        Box::new(as_of.recorded_at),
        Box::new(threshold.max(1) as i64),
    ];
    arguments.extend(
        EXPERIENCE_KINDS
            .iter()
            .map(|kind| Box::new(kind.to_string()) as Box<dyn rusqlite::ToSql>),
    );
    let rows = statement.query_map(params_from_iter(arguments.iter()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
    })?;
    let mut labels = HashMap::new();
    for row in rows {
        let (label, count) = row?;
        labels.insert(label, count);
    }
    Ok(labels)
}

/// Finds timestamps shared by so many records that they date an import.
fn bulk_import_timestamps(
    connection: &Connection,
    space_id: &str,
    as_of: AsOf,
    threshold: usize,
) -> Result<HashSet<Timestamp>, TemplateError> {
    let mut statement = connection.prepare(
        "SELECT valid_from
         FROM node_versions
         WHERE space_id = ?1
           AND valid_from <= ?2
           AND (valid_to IS NULL OR ?2 < valid_to)
           AND recorded_at <= ?3
           AND (superseded_at IS NULL OR ?3 < superseded_at)
         GROUP BY valid_from
         HAVING COUNT(*) >= ?4",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            space_id,
            as_of.valid_at,
            as_of.recorded_at,
            threshold.max(1) as i64
        ],
        |row| row.get::<_, Timestamp>(0),
    )?;
    let mut timestamps = HashSet::new();
    for row in rows {
        timestamps.insert(row?);
    }
    Ok(timestamps)
}

fn kind_placeholders(start: usize) -> String {
    (0..EXPERIENCE_KINDS.len())
        .map(|offset| format!("?{}", start + offset))
        .collect::<Vec<_>>()
        .join(", ")
}

fn id_placeholders(start: usize, count: usize) -> String {
    (0..count)
        .map(|offset| format!("?{}", start + offset))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Splits a query into the terms both the seed search and the evidence bar use.
fn query_terms(query: &str) -> Vec<String> {
    let normalized = query
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>();
    let mut terms = normalized
        .split_whitespace()
        .map(str::to_owned)
        .filter(|term| term.chars().count() >= 2)
        .collect::<Vec<_>>();
    terms.sort();
    terms.dedup();
    terms
}

/// How many of the query's terms a record must carry to count as evidence.
///
/// A single-word query can only ask for that word. Beyond that, one term out of
/// several is a coincidence, so at least two are always required.
fn required_terms(total: usize) -> usize {
    if total <= 1 {
        return total;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "term counts are small and the ceiling is taken of a positive value"
    )]
    let share = (total as f64 * REQUIRED_TERM_SHARE).ceil() as usize;
    share.max(2)
}

/// Counts how many of `terms` appear in a record's own text.
fn terms_present(terms: &[String], label: &str, body: &str) -> usize {
    let haystack = query_terms(&format!("{label} {body}"))
        .into_iter()
        .collect::<HashSet<_>>();
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count()
}

/// Builds an FTS5 expression from untrusted text without string interpolation risk.
fn fts_expression(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn significant_tokens(text: &str) -> HashSet<String> {
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
        .filter(|token| token.chars().count() >= 3)
        .map(str::to_owned)
        .collect()
}

fn dice(left: &HashSet<String>, right: &HashSet<String>) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    2.0 * left.intersection(right).count() as f64 / (left.len() + right.len()) as f64
}

fn iso_date(timestamp: Timestamp) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || "unknown".to_owned(),
        |moment| moment.format("%Y-%m-%d").to_string(),
    )
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let mut truncated = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        truncated.push('…');
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Agent, Attribution, NewEdge, NewNode, NodeReplacement, Session, Space, SpaceKind,
        SqliteStorage, Storage,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SPACE_ID: &str = "templates-space";
    const AGENT_ID: &str = "templates-agent";
    const SESSION_ID: &str = "templates-session";
    const NOW: Timestamp = 2_000_000;
    static NEXT_TEST_DATABASE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabase {
        path: PathBuf,
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.path.as_os_str().to_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(path));
            }
        }
    }

    fn test_database_path(_name: &str) -> PathBuf {
        let _sequence = NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed);
        crate::test_support::temp_path("brain-templates")
    }

    fn attribution(recorded_at: Timestamp) -> Attribution {
        Attribution {
            agent_id: AGENT_ID.to_owned(),
            session_id: SESSION_ID.to_owned(),
            recorded_at,
        }
    }

    fn node(id: &str, kind: NodeKind, label: &str, valid_from: Timestamp) -> NewNode {
        NewNode {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            kind,
            label: label.to_owned(),
            body: label.to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from,
            valid_to: None,
            attribution: attribution(valid_from),
            salience: 1.0,
            source_weight: 1.0,
        }
    }

    fn edge(id: &str, from: &str, to: &str, relation: &str) -> NewEdge {
        NewEdge {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            from_node_id: from.to_owned(),
            to_node_id: to.to_owned(),
            relation: relation.to_owned(),
            weight: 0.9,
            confidence: 0.9,
            metadata_json: "{}".to_owned(),
            valid_from: 100,
            valid_to: None,
            attribution: attribution(100),
            salience: 1.0,
        }
    }

    /// Three independent past migrations with six directly matching records.
    ///
    /// This is deliberately just above the public sufficiency bar: a positive
    /// template test must prove a repeatable pattern, not weaken the bar to make
    /// an undersized fixture pass.
    fn seeded_database(name: &str) -> (TestDatabase, Connection) {
        let path = test_database_path(name);
        let database = TestDatabase { path: path.clone() };
        let storage = SqliteStorage::open(&path).expect("open templates database");
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
                    label: "Templates".to_owned(),
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
            .expect("create templates context");
        storage
            .insert_nodes(&[
                node(
                    "hub-one",
                    NodeKind::Session,
                    "sqlite migration run one",
                    1_000,
                ),
                node(
                    "decision-one",
                    NodeKind::Decision,
                    "Run the sqlite migration inside one transaction",
                    1_010,
                ),
                node(
                    "error-one",
                    NodeKind::Error,
                    "sqlite migration failed on a locked database file",
                    1_020,
                ),
                node(
                    "hub-two",
                    NodeKind::Session,
                    "sqlite migration run two",
                    2_000,
                ),
                node(
                    "decision-two",
                    NodeKind::Decision,
                    "Run the sqlite migration inside one transaction again",
                    2_010,
                ),
                node(
                    "memory-two",
                    NodeKind::Memory,
                    "Back up the sqlite database before every migration",
                    2_020,
                ),
                node(
                    "hub-three",
                    NodeKind::Session,
                    "sqlite migration run three",
                    3_000,
                ),
                node(
                    "decision-three",
                    NodeKind::Decision,
                    "Run the sqlite migration inside one transaction once more",
                    3_010,
                ),
                node(
                    "memory-three",
                    NodeKind::Memory,
                    "Verify the sqlite migration after the transaction commits",
                    3_020,
                ),
            ])
            .expect("insert templates nodes");
        storage
            .insert_edges(&[
                edge("edge-1", "hub-one", "decision-one", "decided"),
                edge("edge-2", "hub-one", "error-one", "encountered"),
                edge("edge-3", "hub-two", "decision-two", "decided"),
                edge("edge-4", "hub-two", "memory-two", "relates_to"),
                edge("edge-5", "hub-three", "decision-three", "decided"),
                edge("edge-6", "hub-three", "memory-three", "relates_to"),
            ])
            .expect("insert templates edges");
        storage.flush().expect("flush templates database");
        drop(storage);
        let connection = Connection::open(&path).expect("open raw connection");
        (database, connection)
    }

    #[test]
    fn similar_groups_records_into_separate_past_episodes() {
        let (_database, connection) = seeded_database("similar");
        let report = similar(
            &connection,
            SPACE_ID,
            "sqlite migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("find similar work");

        assert_eq!(report.episodes.len(), 3, "three hubs must stay separate");
        assert!(report.episodes.iter().all(|episode| episode.anchored));
        assert!(
            report
                .episodes
                .iter()
                .all(|episode| episode.records.len() == 2),
            "each episode keeps only its own records"
        );
    }

    #[test]
    fn every_template_step_quotes_a_dated_source_record() {
        let (_database, connection) = seeded_database("provenance");
        let report = template(
            &connection,
            SPACE_ID,
            "sqlite migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("build template");

        assert!(!report.steps.is_empty());
        for step in report.steps.iter().chain(&report.pitfalls) {
            assert!(
                !step.sources.is_empty(),
                "a step without a source is invented"
            );
            for source in &step.sources {
                assert!(
                    !source.node_id.is_empty(),
                    "every source needs an identifier"
                );
                assert_ne!(source.date, "unknown", "every source needs a date");
            }
            assert!(
                step.sources
                    .iter()
                    .any(|source| source.label == step.statement),
                "statements must be quoted from storage, not written"
            );
        }
    }

    #[test]
    fn repeated_step_is_marked_and_ranked_above_single_sightings() {
        let (_database, connection) = seeded_database("repetition");
        let report = template(
            &connection,
            SPACE_ID,
            "sqlite migration transaction",
            &ExperienceOptions::at(NOW),
        )
        .expect("build template");

        let first = report.steps.first().expect("at least one step");
        assert!(first.is_repeated(), "the twice-seen decision must lead");
        assert_eq!(first.episodes, 3);
        assert_eq!(first.sources.len(), 3);
        assert_eq!(
            report.pitfalls.len(),
            1,
            "recorded errors are reported apart from the steps"
        );
    }

    #[test]
    fn thin_experience_refuses_to_produce_a_template() {
        let (_database, connection) = seeded_database("thin");
        let report = template(
            &connection,
            SPACE_ID,
            "kubernetes ingress certificate rotation",
            &ExperienceOptions::at(NOW),
        )
        .expect("build template");

        assert!(report.steps.is_empty(), "no evidence must yield no steps");
        assert!(matches!(report.verdict, EvidenceVerdict::Insufficient(_)));
        let EvidenceVerdict::Insufficient(reason) = &report.verdict else {
            unreachable!("verdict was asserted above");
        };
        assert!(
            reason.contains("directly backs"),
            "the reason must name what is missing: {reason}"
        );
    }

    #[test]
    fn bulk_import_timestamps_are_not_presented_as_work_dates() {
        let (_database, connection) = seeded_database("bulk");
        let mut options = ExperienceOptions::at(NOW);
        options.bulk_import_threshold = 1;
        let report = similar(&connection, SPACE_ID, "sqlite migration", &options)
            .expect("find similar work");

        assert!(
            report
                .episodes
                .iter()
                .flat_map(|episode| &episode.records)
                .all(|record| record.date_confidence == DateConfidence::BulkImport)
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("import timestamp"))
        );
    }

    /// Two sessions joined by a cross link, a record shared by both, and a
    /// heading repeated often enough to qualify as archive boilerplate.
    fn crowded_database(name: &str) -> (TestDatabase, Connection) {
        let path = test_database_path(name);
        let database = TestDatabase { path: path.clone() };
        let storage = SqliteStorage::open(&path).expect("open templates database");
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
                    label: "Templates".to_owned(),
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
            .expect("create templates context");

        let mut nodes = vec![
            node("hub-one", NodeKind::Session, "migration run one", 1_000),
            node(
                "decision-one",
                NodeKind::Decision,
                "Run the migration inside one transaction",
                1_010,
            ),
            node("hub-two", NodeKind::Session, "migration run two", 2_000),
            node(
                "decision-two",
                NodeKind::Decision,
                "Rehearse the migration on a copy first",
                2_010,
            ),
            node(
                "shared-note",
                NodeKind::Memory,
                "Back up the database before the migration",
                2_020,
            ),
        ];
        let mut edges = vec![
            edge("edge-1", "hub-one", "decision-one", "decided"),
            edge("edge-2", "hub-two", "decision-two", "decided"),
            // The shared note belongs to both stretches of work.
            edge("edge-3", "hub-one", "shared-note", "relates_to"),
            edge("edge-4", "hub-two", "shared-note", "relates_to"),
            // A cross link that a transitive grouping would collapse.
            edge("edge-5", "decision-one", "decision-two", "relates_to"),
        ];
        for index in 0..12 {
            let id = format!("filler-{index}");
            nodes.push(node(
                &id,
                NodeKind::Memory,
                "Reusable knowledge about migration",
                3_000 + index,
            ));
            edges.push(edge(
                &format!("edge-f{index}"),
                "hub-one",
                &id,
                "relates_to",
            ));
        }

        storage
            .insert_nodes(&nodes)
            .expect("insert templates nodes");
        storage
            .insert_edges(&edges)
            .expect("insert templates edges");
        storage.flush().expect("flush templates database");
        drop(storage);
        let connection = Connection::open(&path).expect("open raw connection");
        (database, connection)
    }

    #[test]
    fn a_cross_link_does_not_merge_two_sessions_into_one_episode() {
        let (_database, connection) = crowded_database("crosslink");
        let report = similar(
            &connection,
            SPACE_ID,
            "migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("find similar work");

        let merged = report.episodes.iter().any(|episode| {
            let ids = episode
                .records
                .iter()
                .map(|record| record.node_id.as_str())
                .collect::<HashSet<_>>();
            ids.contains("decision-one") && ids.contains("decision-two")
        });
        assert!(
            !merged,
            "a relates_to link between two sessions' records must not fuse their episodes"
        );
    }

    #[test]
    fn a_record_shared_by_two_sessions_does_not_read_as_repeated() {
        let (_database, connection) = crowded_database("shared");
        let context = collect(
            &connection,
            SPACE_ID,
            "migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("collect similar work");
        let (steps, pitfalls) = build_steps(&context.episodes, NOW);

        let shared = steps
            .iter()
            .chain(&pitfalls)
            .find(|step| step.statement == "Back up the database before the migration")
            .expect("the shared note must survive as a step");
        assert_eq!(shared.episodes, 2, "it really does belong to two sessions");
        assert_eq!(shared.sources.len(), 1, "but it is still one record");
        assert!(
            !shared.is_repeated(),
            "one record seen twice is not a repeated pattern"
        );
    }

    #[test]
    fn structural_headers_never_become_template_steps() {
        let (_database, connection) = crowded_database("headers");
        let context = collect(
            &connection,
            SPACE_ID,
            "migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("collect similar work");
        let (steps, pitfalls) = build_steps(&context.episodes, NOW);

        assert!(
            steps
                .iter()
                .chain(&pitfalls)
                .all(|step| step.statement != "Reusable knowledge about migration"),
            "a heading repeated across the archive must never be presented as a step"
        );
        assert!(
            context
                .warnings
                .iter()
                .any(|warning| warning.contains("section header")),
            "the skipped records must be reported, not silently dropped: {:?}",
            context.warnings
        );
    }

    #[test]
    fn one_shared_word_is_not_treated_as_relevant_experience() {
        let (_database, connection) = crowded_database("coverage");
        let report = template(
            &connection,
            SPACE_ID,
            "kubernetes ingress migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("build template");

        assert!(
            report.steps.is_empty(),
            "matching only the word 'migration' must not produce a plan"
        );
        assert!(matches!(report.verdict, EvidenceVerdict::Insufficient(_)));
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| { warning.contains("kubernetes") && warning.contains("ingress") }),
            "the refusal must name the words the archive never saw: {:?}",
            report.warnings
        );
    }

    #[test]
    fn empty_query_is_rejected_before_touching_the_database() {
        let (_database, connection) = seeded_database("empty");
        let error = similar(&connection, SPACE_ID, "   ", &ExperienceOptions::at(NOW))
            .expect_err("empty query must fail");
        assert!(matches!(error, TemplateError::InvalidQuery(_)));
    }

    #[test]
    fn thin_evidence_is_named_and_never_emitted_as_a_template() {
        let (_database, connection) = seeded_database("honest-thin");
        let mut options = ExperienceOptions::at(NOW);
        options.as_of = AsOf::new(2_500, 2_500);

        let similar_report = similar(&connection, SPACE_ID, "sqlite migration", &options)
            .expect("find two past episodes");
        assert!(matches!(similar_report.verdict, EvidenceVerdict::Thin(_)));
        assert_eq!(similar_report.to_json()["status"], "insufficient_evidence");

        let report = template(&connection, SPACE_ID, "sqlite migration", &options)
            .expect("refuse a thin template");
        assert!(matches!(report.verdict, EvidenceVerdict::Thin(_)));
        assert!(report.steps.is_empty());
        assert!(report.pitfalls.is_empty());
        assert!(report.episodes_used.is_empty());
        assert_eq!(report.to_json()["status"], "refused");
        assert!(report.to_json()["verdict_reason"].is_string());
    }

    #[test]
    fn configured_evidence_minima_are_enforced() {
        let (_database, connection) = seeded_database("configured-minima");
        let mut options = ExperienceOptions::at(NOW);
        options.min_episodes = 99;
        options.min_records = 99;

        let report = template(&connection, SPACE_ID, "sqlite migration", &options)
            .expect("apply strict minima");
        let EvidenceVerdict::Insufficient(reason) = &report.verdict else {
            panic!("strict caller minima must refuse the template");
        };
        assert!(reason.contains("99 records"), "unexpected reason: {reason}");
        assert!(
            reason.contains("99 such episodes"),
            "unexpected reason: {reason}"
        );
        assert!(report.steps.is_empty());
        assert_eq!(report.to_json()["status"], "refused");
    }

    #[test]
    fn unanchored_singletons_are_not_independent_past_episodes() {
        let (database, connection) = seeded_database("unanchored");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        let nodes = (0..15)
            .map(|index| {
                node(
                    &format!("orphan-{index}"),
                    NodeKind::Decision,
                    &format!("orphan recovery procedure {index}"),
                    10_000 + index,
                )
            })
            .collect::<Vec<_>>();
        storage.insert_nodes(&nodes).expect("insert orphan records");
        storage.flush().expect("flush orphan records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = template(
            &connection,
            SPACE_ID,
            "orphan recovery",
            &ExperienceOptions::at(NOW),
        )
        .expect("evaluate orphan records");
        let EvidenceVerdict::Insufficient(reason) = &report.verdict else {
            panic!("unanchored records must not be called independent episodes");
        };
        assert!(
            reason.contains("in an anchored episode"),
            "unexpected reason: {reason}"
        );
        assert!(report.steps.is_empty());
        assert_eq!(report.to_json()["status"], "refused");
    }

    #[test]
    fn detached_records_cannot_pad_the_anchored_record_bar() {
        let (database, connection) = seeded_database("detached-padding");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        let detached = (0..3)
            .map(|index| {
                node(
                    &format!("detached-{index}"),
                    NodeKind::Decision,
                    &format!("Run detached transaction procedure {index}"),
                    9_000 + index,
                )
            })
            .collect::<Vec<_>>();
        storage
            .insert_nodes(&detached)
            .expect("insert detached records");
        storage.flush().expect("flush detached records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = template(
            &connection,
            SPACE_ID,
            "run transaction",
            &ExperienceOptions::at(NOW),
        )
        .expect("evaluate detached padding");
        let EvidenceVerdict::Thin(reason) = &report.verdict else {
            panic!("three detached records must not pad three anchored records to six");
        };
        assert!(
            reason.contains("only 3 query-backed records"),
            "unexpected reason: {reason}"
        );
        assert!(report.steps.is_empty());
    }

    #[test]
    fn detached_records_do_not_contaminate_a_sufficient_template() {
        let (database, connection) = seeded_database("detached-contamination");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        storage
            .insert_node(&node(
                "detached-noise",
                NodeKind::Decision,
                "SQLite migration unrelated detached instruction",
                9_500,
            ))
            .expect("insert detached noise");
        storage.flush().expect("flush detached noise");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = template(
            &connection,
            SPACE_ID,
            "sqlite migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("build anchored template");
        assert_eq!(report.verdict, EvidenceVerdict::Sufficient);
        assert!(
            report
                .steps
                .iter()
                .chain(&report.pitfalls)
                .all(|step| step.statement != "SQLite migration unrelated detached instruction")
        );
        assert!(
            report
                .episodes_used
                .iter()
                .all(|episode| episode.starts_with("hub-"))
        );
    }

    #[test]
    fn query_coverage_respects_token_boundaries() {
        let (database, connection) = seeded_database("token-boundaries");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        let mut records = Vec::new();
        let mut links = Vec::new();
        for (index, hub) in ["hub-one", "hub-two", "hub-three"].into_iter().enumerate() {
            for offset in 0..2 {
                let id = format!("trust-{index}-{offset}");
                records.push(node(
                    &id,
                    NodeKind::Decision,
                    "Trust sqlite migration before release",
                    20_000 + (index * 10 + offset) as Timestamp,
                ));
                links.push(edge(
                    &format!("trust-edge-{index}-{offset}"),
                    hub,
                    &id,
                    "decided",
                ));
            }
        }
        storage
            .insert_nodes(&records)
            .expect("insert boundary records");
        storage
            .insert_edges(&links)
            .expect("attach boundary records");
        storage.flush().expect("flush boundary records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = template(
            &connection,
            SPACE_ID,
            "rust migration",
            &ExperienceOptions::at(NOW),
        )
        .expect("evaluate token boundaries");
        assert!(report.steps.is_empty());
        assert!(matches!(report.verdict, EvidenceVerdict::Insufficient(_)));
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("rust")),
            "the missing whole token must be named: {:?}",
            report.warnings
        );
    }

    #[test]
    fn zero_weight_links_cannot_manufacture_anchored_evidence() {
        let (database, connection) = seeded_database("zero-weight-anchors");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        let mut records = Vec::new();
        let mut links = Vec::new();
        for (index, hub) in ["hub-one", "hub-two", "hub-three"].into_iter().enumerate() {
            for offset in 0..2 {
                let id = format!("zero-evidence-{index}-{offset}");
                records.push(node(
                    &id,
                    NodeKind::Decision,
                    "Repeat zero anchor procedure",
                    30_000 + (index * 10 + offset) as Timestamp,
                ));
                let mut link = edge(&format!("zero-edge-{index}-{offset}"), hub, &id, "decided");
                link.weight = 0.0;
                links.push(link);
            }
        }
        storage
            .insert_nodes(&records)
            .expect("insert zero-weight records");
        storage
            .insert_edges(&links)
            .expect("attach zero-weight records");
        storage.flush().expect("flush zero-weight records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = template(
            &connection,
            SPACE_ID,
            "zero anchor",
            &ExperienceOptions::at(NOW),
        )
        .expect("evaluate zero-weight evidence");
        assert!(matches!(report.verdict, EvidenceVerdict::Insufficient(_)));
        assert!(report.steps.is_empty());
    }

    #[test]
    fn bounded_episode_fanout_is_reported() {
        let (database, connection) = seeded_database("fanout-warning");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        let mut records = vec![
            node("fanout-hub", NodeKind::Session, "fanout session", 40_000),
            node(
                "fanout-seed",
                NodeKind::Decision,
                "fanout marker decision",
                40_001,
            ),
        ];
        let mut links = vec![edge(
            "fanout-edge-seed",
            "fanout-hub",
            "fanout-seed",
            "decided",
        )];
        for index in 0..41 {
            let id = format!("fanout-member-{index:02}");
            records.push(node(
                &id,
                NodeKind::Memory,
                &format!("session context member {index}"),
                40_100 + index,
            ));
            links.push(edge(
                &format!("fanout-edge-{index:02}"),
                "fanout-hub",
                &id,
                "relates_to",
            ));
        }
        storage
            .insert_nodes(&records)
            .expect("insert fanout records");
        storage.insert_edges(&links).expect("insert fanout edges");
        storage.flush().expect("flush fanout records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");

        let report = similar(
            &connection,
            SPACE_ID,
            "fanout marker",
            &ExperienceOptions::at(NOW),
        )
        .expect("read bounded episode");
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("40 neighbours")
                    && warning.contains("not exhaustive")),
            "fanout truncation must be visible: {:?}",
            report.warnings
        );
    }

    #[test]
    fn both_temporal_axes_filter_nodes_and_edges() {
        let (database, connection) = seeded_database("bitemporal");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");

        let mut future = node(
            "future-node",
            NodeKind::Decision,
            "quantum unborn chronometer",
            NOW + 100,
        );
        future.attribution.recorded_at = NOW + 100;
        let mut expired = node(
            "expired-node",
            NodeKind::Decision,
            "quantum retired chronometer",
            NOW - 100,
        );
        expired.valid_to = Some(NOW - 1);
        let original = node(
            "revision-node",
            NodeKind::Decision,
            "temporal snapshot original",
            100,
        );
        let edge_seed = node(
            "future-edge-seed",
            NodeKind::Decision,
            "future edge sentinel",
            100,
        );
        let edge_neighbour = node(
            "future-edge-neighbour",
            NodeKind::Memory,
            "secret graph neighbour",
            100,
        );
        storage
            .insert_nodes(&[future, expired, original, edge_seed, edge_neighbour])
            .expect("insert temporal records");
        storage
            .replace_node(
                SPACE_ID,
                "revision-node",
                &NodeReplacement {
                    kind: NodeKind::Decision,
                    label: "temporal snapshot replacement".to_owned(),
                    body: "temporal snapshot replacement".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 100,
                    attribution: attribution(NOW + 200),
                },
            )
            .expect("insert future-known revision");
        let mut future_edge = edge(
            "future-edge",
            "future-edge-seed",
            "future-edge-neighbour",
            "relates_to",
        );
        future_edge.valid_from = NOW + 100;
        future_edge.attribution = attribution(NOW + 100);
        storage
            .insert_edge(&future_edge)
            .expect("insert future edge");
        storage.flush().expect("flush temporal records");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");
        let options = ExperienceOptions::at(NOW);

        for query in ["quantum unborn chronometer", "quantum retired chronometer"] {
            let report = similar(&connection, SPACE_ID, query, &options)
                .expect("run temporal exclusion query");
            assert_eq!(report.matched_records, 0, "{query:?} leaked at as_of");
        }

        let prior = similar(&connection, SPACE_ID, "temporal snapshot", &options)
            .expect("read knowledge before future supersession");
        let records = prior
            .episodes
            .iter()
            .flat_map(|episode| &episode.records)
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label, "temporal snapshot original");
        assert_eq!(records[0].revision, 1);

        let edge_report = similar(&connection, SPACE_ID, "future edge sentinel", &options)
            .expect("query before edge exists");
        let ids = edge_report
            .episodes
            .iter()
            .flat_map(|episode| &episode.records)
            .map(|record| record.node_id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(ids, HashSet::from(["future-edge-seed"]));
    }

    /// A record titled with a bare path says where work happened, not what was
    /// done. One reached a live template as step four, reading `/home/<user>`
    /// — useless as instruction and someone's directory layout in a report.
    #[test]
    fn a_bare_path_is_not_a_step() {
        for label in [
            "/home/someone/projects/thing",
            "~/work/repo",
            "./scripts/build",
            "src/parser/lexer.rs",
        ] {
            assert!(
                label_is_only_a_path(label),
                "{label} is a location, not a step"
            );
        }
        for label in [
            "Fixed the parser in src/lexer.rs",
            "Перегрев VS Code",
            "vscode-thermal-fix",
            "docs/schema.md explains the interval semantics",
        ] {
            assert!(
                !label_is_only_a_path(label),
                "{label} says something and must survive"
            );
        }
    }

    #[test]
    fn trusted_seeds_require_signal_provenance_and_keep_it_in_json() {
        let (database, connection) = seeded_database("trusted-seed");
        drop(connection);
        let storage = SqliteStorage::open(&database.path).expect("reopen templates storage");
        storage
            .insert_node(&node(
                "semantic-only",
                NodeKind::Decision,
                "Deploy the lunar database safely",
                5_000,
            ))
            .expect("insert semantic-only record");
        storage.flush().expect("flush semantic-only record");
        drop(storage);
        let connection = Connection::open(&database.path).expect("reopen raw connection");
        let mut options = ExperienceOptions::at(NOW);
        options.trusted_seeds.push(ExperienceSeed {
            node_id: "semantic-only".to_owned(),
            relevance: 0.71,
            signal: ExperienceSeedSignal::Vector,
        });
        options
            .degradation_warnings
            .push("test-only semantic degradation that must remain machine-visible".to_owned());

        let report = similar(
            &connection,
            SPACE_ID,
            "unrelated kubernetes ingress",
            &options,
        )
        .expect("use a typed vector seed");
        let record = report
            .episodes
            .iter()
            .flat_map(|episode| &episode.records)
            .find(|record| record.node_id == "semantic-only")
            .expect("vector seed must survive lexical coverage");
        assert!(record.query_backed);
        assert_eq!(record.found_by, vec!["vector"]);
        assert_eq!(record.relevance, 0.71);
        let value = report.to_json();
        assert_eq!(value["schema_version"], REPORT_SCHEMA_VERSION);
        assert_eq!(value["space"], SPACE_ID);
        assert_eq!(value["as_of"]["valid_at"], NOW);
        assert_eq!(value["as_of"]["recorded_at"], NOW);
        assert!(
            value["warnings"]
                .as_array()
                .expect("warnings array")
                .iter()
                .any(|warning| warning
                    .as_str()
                    .is_some_and(|warning| warning.contains("test-only")))
        );
        let source = &value["episodes"][0]["records"][0];
        for key in [
            "id",
            "space",
            "revision",
            "source",
            "valid_from",
            "valid_to",
            "recorded_at",
            "superseded_at",
            "source_agent",
            "source_session",
            "found_by",
            "query_backed",
        ] {
            assert!(source.get(key).is_some(), "missing provenance key {key}");
        }
    }
}
