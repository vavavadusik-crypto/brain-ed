//! Hybrid BM25/vector/exact search with reciprocal-rank fusion.

use std::collections::{HashMap, HashSet};

use crate::profile::{Profile, QualitySettings};

use thiserror::Error;

use crate::embeddings::{Embedder, EmbeddingError};
use crate::salience::{DAY, SalienceInput, salience};
use crate::{
    AsOf, Attribution, HybridSearchHit, NewEmbedding, Node, SearchLayers, SearchOutput, Storage,
    StorageError, Timestamp,
};

const K_RRF: f64 = 60.0;
const EXACT_BONUS: f64 = 0.5;
const TAU_RECENCY: f64 = 30.0 * DAY as f64;
const L_TARGET: f64 = 512.0;
/// Below this many tokens a record is a fragment, not an answer.
const L_SUBSTANTIAL: f64 = 24.0;
const MIN_SIGNALS: usize = 2;
const MIN_FINAL: f64 = 0.02;
const DEDUP_SIM: f64 = 0.92;
const MAX_CANDIDATES: usize = 200;
/// How many matched records the graph walk starts from.
const GRAPH_SEEDS: usize = 12;
/// Paths read per seed. Hub records have hundreds of edges — one of them has
/// 205 — and following all of them would bury the answer in neighbours.
const GRAPH_FANOUT: usize = 40;
/// How much a record's claim weakens per hop away from a real match.
const GRAPH_DECAY: f64 = 0.45;
/// A neighbour's share of the score a direct match would earn at the same rank.
const GRAPH_WEIGHT: f64 = 0.5;
/// Most neighbours admitted to the candidate pool, however wide the profile.
const GRAPH_ADMIT: usize = 24;
/// Below this many direct hits, the query is worth widening with corpus terms.
const EXPANSION_TRIGGER: usize = 8;
/// Retrieved records a term must appear in before it counts as topic vocabulary.
const EXPANSION_MIN_DOCUMENTS: usize = 3;
/// Terms borrowed from the corpus per query.
const EXPANSION_MAX_TERMS: usize = 5;
/// Shorter words are grammar rather than subject matter.
const EXPANSION_MIN_TERM_CHARS: usize = 4;
/// How much weaker a hit found through borrowed vocabulary ranks.
const EXPANSION_DISCOUNT: f64 = 0.5;
/// Nodes expanded at each layer, across every seed. This bounds a four-hop
/// profile even when the seed is a hub.
const GRAPH_FRONTIER: usize = 24;
/// Public settings allow at most this depth; callers constructing settings in
/// code are capped too so an accidental huge value cannot explode the walk.
const GRAPH_MAX_HOPS: u32 = 4;

/// Query coordinates and layer scope supplied to hybrid search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchQuery {
    /// Isolated memory space.
    pub space_id: String,
    /// Natural-language query text.
    pub text: String,
    /// Bi-temporal query point.
    pub as_of: AsOf,
    /// Salience layers eligible for retrieval.
    pub layers: SearchLayers,
    /// Restrict results to records written through the memory API.
    pub memory_only: bool,
    /// Record hits as accessed (a write). Off for read-only retrieval.
    pub record_access: bool,
    /// Match lexical token prefixes for incremental search boxes.
    pub prefix_matching: bool,
}

impl SearchQuery {
    /// Creates an ordinary hot-layer query.
    #[must_use]
    pub fn new(space_id: impl Into<String>, text: impl Into<String>, as_of: AsOf) -> Self {
        Self {
            space_id: space_id.into(),
            text: text.into(),
            as_of,
            layers: SearchLayers::HOT,
            memory_only: false,
            record_access: true,
            prefix_matching: false,
        }
    }

    /// Expands or narrows the salience-layer scope.
    #[must_use]
    pub const fn with_layers(mut self, layers: SearchLayers) -> Self {
        self.layers = layers;
        self
    }

    /// Restricts retrieval to user-memory records and excludes indexed code/docs.
    #[must_use]
    pub const fn memory_only(mut self) -> Self {
        self.memory_only = true;
        self
    }

    /// Enables prefix matching while keeping the ordinary hybrid ranking.
    #[must_use]
    pub const fn with_prefix_matching(mut self) -> Self {
        self.prefix_matching = true;
        self
    }

    /// Leaves access statistics untouched so the search performs no write.
    #[must_use]
    pub const fn without_access_tracking(mut self) -> Self {
        self.record_access = false;
        self
    }
}

/// Failure returned by embedding, candidate retrieval, or query validation.
#[derive(Debug, Error)]
pub enum SearchError {
    /// The query does not contain searchable text or a space identifier.
    #[error("invalid search query: {0}")]
    InvalidQuery(String),

    /// An indexing operation was requested without an embedding backend.
    #[error("embedding backend is unavailable: {0}")]
    EmbeddingUnavailable(String),

    /// Embedding generation failed.
    #[error(transparent)]
    Embedding(#[from] EmbeddingError),

    /// Candidate retrieval or embedding persistence failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Search service composed only from storage and embedding abstractions.
pub struct HybridSearch<'a, S: ?Sized> {
    storage: &'a S,
    embedder: Option<&'a dyn Embedder>,
    vector_warning: Option<String>,
}

impl<'a, S> HybridSearch<'a, S>
where
    S: Storage + ?Sized,
{
    /// Borrows the pluggable storage and embedding backends.
    #[must_use]
    pub fn new<E: Embedder>(storage: &'a S, embedder: &'a E) -> Self {
        Self {
            storage,
            embedder: Some(embedder),
            vector_warning: None,
        }
    }

    /// Builds a lexical/exact searcher with a caller-visible degradation reason.
    #[must_use]
    pub fn without_vectors(storage: &'a S, reason: impl Into<String>) -> Self {
        Self {
            storage,
            embedder: None,
            vector_warning: Some(reason.into()),
        }
    }

    /// Builds a searcher from an optional runtime-loaded embedding backend.
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

    /// Embeds and persists one immutable node version through `Storage`.
    ///
    /// # Errors
    ///
    /// Returns an error when embedding generation, dimension validation, or
    /// storage fails.
    pub fn index_node(
        &self,
        node: &Node,
        attribution: Attribution,
        embedded_at: Timestamp,
    ) -> Result<(), SearchError> {
        let embedder = self.embedder.ok_or_else(|| {
            SearchError::EmbeddingUnavailable(
                self.vector_warning
                    .clone()
                    .unwrap_or_else(|| "no backend was configured".to_owned()),
            )
        })?;
        let text = format!("{}\n{}", node.label, node.body);
        let vector = embedder.embed(&text)?;
        if vector.len() != embedder.dimensions() {
            return Err(EmbeddingError::Dimensions {
                expected: embedder.dimensions(),
                found: vector.len(),
            }
            .into());
        }
        self.storage.upsert_embedding(&NewEmbedding {
            node_version_id: node.version_id,
            space_id: node.space_id.clone(),
            model_id: embedder.model_id().to_owned(),
            vector,
            embedded_at,
            attribution,
        })?;
        Ok(())
    }

    /// Runs BM25, vector, and exact signals, then applies RRF, quality gates,
    /// semantic deduplication, and a hard token budget.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty query, embedding failure, or storage failure.
    pub fn search(
        &self,
        query: &SearchQuery,
        budget_tokens: usize,
    ) -> Result<SearchOutput, SearchError> {
        self.search_with(
            query,
            &QualitySettings {
                budget_tokens,
                ..Profile::default().settings()
            },
        )
    }

    /// Pulls in records connected to the ones the query already matched.
    ///
    /// Static embeddings score words, not meaning, so "жара процессора" finds
    /// nothing about overheating however low the cosine cutoff goes. The graph
    /// carries the association that the vectors miss: whatever the query did
    /// match is connected to the record that answers it.
    ///
    /// Returns how many candidates received graph evidence. The public
    /// `graph_reached` count is computed later from records that actually
    /// survive filtering, deduplication, and the token budget.
    fn walk_graph(
        &self,
        query: &SearchQuery,
        quality: &QualitySettings,
        candidates: &mut HashMap<String, Candidate>,
        warnings: &mut Vec<String>,
    ) -> Result<usize, SearchError> {
        if quality.graph_hops == 0 {
            return Ok(0);
        }
        if candidates.is_empty() {
            warnings.push(
                "graph walk could not start because direct retrieval found no candidate".to_owned(),
            );
            return Ok(0);
        }
        // Seeds are the records the query matched outright, best first. Walking
        // from every candidate would drown the answer in second-hand material.
        // Only confident matches may seed the walk. Following a weak match
        // multiplies the mistake: asking about processor heat with nothing
        // solid to start from returned unrelated code — ocr_image(),
        // probe_video() — dragged in from whatever tenuously matched. An empty
        // answer is better than a confident irrelevant one.
        let direct_matches = candidates
            .values()
            .filter(|candidate| candidate.is_confident_match(quality))
            .map(|candidate| candidate.node.id.clone())
            .collect::<HashSet<_>>();
        let mut seeds = candidates
            .values()
            .filter(|candidate| direct_matches.contains(&candidate.node.id))
            .map(|candidate| (candidate.node.id.clone(), candidate.seed_strength()))
            .collect::<Vec<_>>();
        if seeds.is_empty() {
            warnings.push(
                "graph walk skipped: no direct match was strong enough to use as a seed".to_owned(),
            );
            return Ok(0);
        }
        seeds.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        let mut graph_truncated = seeds.len() > GRAPH_SEEDS;
        seeds.truncate(GRAPH_SEEDS);

        let max_hops = quality.graph_hops.min(GRAPH_MAX_HOPS);
        let admission_limit = GRAPH_ADMIT.min(quality.candidates_per_signal);
        if quality.graph_hops > GRAPH_MAX_HOPS {
            warnings.push(format!(
                "graph walk depth {} was capped at {GRAPH_MAX_HOPS}",
                quality.graph_hops
            ));
        }

        // Use the live edge versions for every layer. Materialized paths are a
        // useful current-state acceleration for reporting and maintenance, but
        // they carry no AsOf coordinate and cannot answer historical search
        // correctly. The same undirected rule is applied at every depth.
        let mut frontier = seeds
            .iter()
            .map(|(node_id, seed_strength)| (node_id.clone(), *seed_strength))
            .collect::<Vec<_>>();
        let mut best_depth = direct_matches
            .iter()
            .map(|node_id| (node_id.clone(), 0_u32))
            .collect::<HashMap<_, _>>();
        let mut reached: HashMap<String, (u32, f64)> = HashMap::new();
        let mut connections_seen = 0usize;
        for distance in 1..=max_hops {
            let mut next = HashMap::<String, f64>::new();
            let frontier_strengths = frontier.iter().cloned().collect::<HashMap<_, _>>();
            let frontier_ids = frontier_strengths.keys().cloned().collect::<Vec<_>>();
            let mut fanout_seen = HashMap::<String, usize>::new();
            for neighbor in self.storage.graph_neighbors(
                &query.space_id,
                &frontier_ids,
                query.as_of,
                query.layers,
                query.memory_only,
                GRAPH_FANOUT + 1,
            )? {
                let seen = fanout_seen
                    .entry(neighbor.source_node_id.clone())
                    .or_default();
                *seen += 1;
                if *seen > GRAPH_FANOUT {
                    graph_truncated = true;
                    continue;
                }
                connections_seen += 1;
                if direct_matches.contains(&neighbor.node_id) {
                    continue;
                }
                if best_depth
                    .get(&neighbor.node_id)
                    .is_some_and(|known| *known < distance)
                {
                    continue;
                }
                best_depth
                    .entry(neighbor.node_id.clone())
                    .or_insert(distance);
                let route_strength = frontier_strengths
                    .get(&neighbor.source_node_id)
                    .copied()
                    .unwrap_or(0.0);
                let strength = route_strength * neighbor.edge_weight * GRAPH_DECAY;
                let reached_entry = reached
                    .entry(neighbor.node_id.clone())
                    .or_insert((distance, strength));
                if distance < reached_entry.0
                    || (distance == reached_entry.0 && strength > reached_entry.1)
                {
                    *reached_entry = (distance, strength);
                }
                let next_strength = next.entry(neighbor.node_id).or_insert(strength);
                if strength > *next_strength {
                    *next_strength = strength;
                }
            }
            let mut next_ranked = next.into_iter().collect::<Vec<_>>();
            next_ranked.sort_by(|left, right| {
                right
                    .1
                    .total_cmp(&left.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            graph_truncated |= next_ranked.len() > GRAPH_FRONTIER;
            next_ranked.truncate(GRAPH_FRONTIER);
            frontier = next_ranked;
            if frontier.is_empty() {
                break;
            }
        }
        if connections_seen == 0 {
            if graph_truncated {
                warnings.push(format!(
                    "graph walk was bounded to {GRAPH_SEEDS} seeds, {GRAPH_FANOUT} neighbours per node, {GRAPH_FRONTIER} frontier nodes, and {admission_limit} admitted candidates; graph_hops is a maximum depth, not exhaustive traversal"
                ));
            }
            // Only worth saying when the graph has connections that this query
            // failed to reach. On a database with no edges yet — a first run,
            // most obviously — the walk finding nothing is the expected state,
            // and announcing it as a degradation greets a new user with alarm.
            if self.storage.has_edges(&query.space_id)? {
                warnings.push(
                    "graph walk found no eligible active connection from the retained seed set at the requested time and scope"
                        .to_owned(),
                );
            }
            return Ok(0);
        }

        let mut ranked = reached.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .1
                .1
                .total_cmp(&left.1.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        // Neighbours are capped well below the other signals. A hub record has
        // hundreds of them, and an answer made mostly of associations is worse
        // than a short answer made of matches.
        graph_truncated |= ranked.len() > admission_limit;
        ranked.truncate(admission_limit);
        if graph_truncated {
            warnings.push(format!(
                "graph walk was bounded to {GRAPH_SEEDS} seeds, {GRAPH_FANOUT} neighbours per node, {GRAPH_FRONTIER} frontier nodes, and {admission_limit} admitted candidates; graph_hops is a maximum depth, not exhaustive traversal"
            ));
        }

        let mut added = 0usize;
        for (rank, (node_id, (distance, strength))) in ranked.into_iter().enumerate() {
            if candidates
                .get(&node_id)
                .is_some_and(|candidate| candidate.is_confident_match(quality))
            {
                continue;
            }
            if !candidates.contains_key(&node_id) {
                let Some(node) = self
                    .storage
                    .node_as_of(&query.space_id, &node_id, query.as_of)?
                else {
                    continue;
                };
                if query.memory_only && !is_memory_record(&node) {
                    continue;
                }
                candidates.insert(node_id.clone(), Candidate::new(node));
            }
            let candidate = candidates
                .get_mut(&node_id)
                .expect("candidate was present or inserted");
            candidate.graph_rank = Some(rank + 1);
            candidate.graph_distance = Some(distance);
            candidate.graph_strength = Some(strength);
            added += 1;
        }
        Ok(added)
    }

    /// Runs a search at an explicit point on the cost/accuracy curve.
    ///
    /// The knobs used to be compile-time constants, which meant every user got
    /// the same trade-off whether they wanted speed or precision.
    pub fn search_with(
        &self,
        query: &SearchQuery,
        quality: &QualitySettings,
    ) -> Result<SearchOutput, SearchError> {
        let budget_tokens = quality.budget_tokens;
        if query.space_id.trim().is_empty() {
            return Err(SearchError::InvalidQuery(
                "space_id cannot be empty".to_owned(),
            ));
        }
        if query.text.trim().is_empty() {
            return Err(SearchError::InvalidQuery(
                "query text cannot be empty".to_owned(),
            ));
        }
        let mut warnings = self.vector_warning.clone().into_iter().collect::<Vec<_>>();
        if self.embedder.is_none() && warnings.is_empty() {
            warnings.push("vector search unavailable; using BM25 + exact matches only".to_owned());
        }
        let fts = fts_query_with_prefix(&query.text, query.prefix_matching);
        if budget_tokens == 0 || fts.is_empty() {
            return Ok(SearchOutput {
                hits: Vec::new(),
                used_tokens: 0,
                vector_search: false,
                graph_reached: 0,
                warnings,
            });
        }

        let seed_terms = graph_seed_terms(&query.text);
        let mut lexical_hits = if fts.is_empty() {
            Vec::new()
        } else {
            self.storage.search_nodes_in_layers(
                &query.space_id,
                &fts,
                query.as_of,
                query.layers,
                quality.candidates_per_signal,
            )?
        };
        // Static embeddings score words rather than meaning, so a question
        // phrased differently from the record answering it finds little. The
        // corpus itself supplies the missing vocabulary: terms that cluster in
        // the few records the query did reach are the words this collection
        // actually uses for the topic. Only worth doing when the direct attempt
        // came back thin — otherwise it adds noise to an answer already found.
        let expanded_with = if !fts.is_empty() && lexical_hits.len() < EXPANSION_TRIGGER {
            expansion_terms(&query.text, &lexical_hits)
        } else {
            Vec::new()
        };
        {
            if !expanded_with.is_empty() {
                let widened = fts_query(&expanded_with.join(" "));
                let extra = self.storage.search_nodes_in_layers(
                    &query.space_id,
                    &widened,
                    query.as_of,
                    query.layers,
                    quality.candidates_per_signal,
                )?;
                let known = lexical_hits
                    .iter()
                    .map(|hit| hit.node.id.clone())
                    .collect::<HashSet<_>>();
                // Borrowed vocabulary is weaker evidence than the user's own
                // words, so an expanded hit enters ranked below every direct one.
                let before = lexical_hits.len();
                lexical_hits.extend(
                    extra
                        .into_iter()
                        .filter(|hit| !known.contains(&hit.node.id))
                        .map(|mut hit| {
                            hit.bm25 *= EXPANSION_DISCOUNT;
                            hit
                        }),
                );
                let added = lexical_hits.len() - before;
                if added > 0 {
                    // Say which words were borrowed. A result the user never
                    // asked for should be explainable, not just present.
                    warnings.push(format!(
                        "few direct matches, so the query was widened with terms this collection uses for the topic: {}",
                        expanded_with.join(", ")
                    ));
                }
            }
        }
        let mut bm25_ranked = lexical_hits
            .into_iter()
            .filter(|hit| !query.memory_only || is_memory_record(&hit.node))
            .map(|hit| {
                let raw = (-hit.bm25).max(f64::MIN_POSITIVE);
                // Length belongs in the cutoff, not only in the ordering: a
                // three-token heading scores high on raw BM25 and would otherwise
                // clear min_bm25 as comfortably as the article that answers the
                // question. Recency and salience stay out of it, so that whether a
                // record is admitted does not depend on when it was last touched.
                let lexical = raw * length_penalty(hit.node.token_len);
                let weighted = lexical * node_boost(&hit.node, query.as_of.recorded_at);
                Bm25Ranked {
                    node: hit.node,
                    lexical,
                    weighted,
                }
            })
            .collect::<Vec<_>>();
        bm25_ranked.sort_by(|left, right| {
            right
                .weighted
                .total_cmp(&left.weighted)
                .then_with(|| left.node.id.cmp(&right.node.id))
        });
        let max_bm25 = bm25_ranked
            .iter()
            .map(|candidate| candidate.lexical)
            .reduce(f64::max)
            .unwrap_or(1.0);

        let mut vector_ranked = Vec::new();
        if let Some(embedder) = self.embedder {
            match embedder.embed(&query.text) {
                Ok(query_vector) if query_vector.len() == embedder.dimensions() => {
                    match self.storage.vector_search(
                        &query.space_id,
                        embedder.model_id(),
                        &query_vector,
                        query.as_of,
                        query.layers,
                        quality.candidates_per_signal,
                    ) {
                        Ok(hits) => {
                            vector_ranked = hits
                                .into_iter()
                                .filter(|hit| {
                                    !query.memory_only || is_memory_record(&hit.node)
                                })
                                .collect();
                        }
                        Err(error) => warnings.push(format!(
                            "vector search failed ({error}); using BM25 + exact matches only"
                        )),
                    }
                }
                Ok(query_vector) => warnings.push(format!(
                    "embedding backend returned {} dimensions; expected {}; using BM25 + exact matches only",
                    query_vector.len(),
                    embedder.dimensions()
                )),
                Err(error) => warnings.push(format!(
                    "embedding generation failed ({error}); using BM25 + exact matches only"
                )),
            }
        }
        vector_ranked.sort_by(|left, right| {
            let left_score = left.cosine * node_boost(&left.node, query.as_of.recorded_at);
            let right_score = right.cosine * node_boost(&right.node, query.as_of.recorded_at);
            right_score
                .total_cmp(&left_score)
                .then_with(|| left.node.id.cmp(&right.node.id))
        });
        let exact = self
            .storage
            .exact_match_nodes(
                &query.space_id,
                &query.text,
                query.as_of,
                query.layers,
                quality.candidates_per_signal,
            )?
            .into_iter()
            .filter(|node| !query.memory_only || is_memory_record(node))
            .collect::<Vec<_>>();
        let vector_search = !vector_ranked.is_empty();
        if self.embedder.is_some() && !vector_search && warnings.is_empty() {
            warnings.push(
                "vector index has no candidates for this model; using BM25 + exact matches only"
                    .to_owned(),
            );
        }

        let mut candidates: HashMap<String, Candidate> = HashMap::new();
        for (index, ranked) in bm25_ranked.into_iter().enumerate() {
            let normalized = if max_bm25 > 0.0 {
                ranked.lexical / max_bm25
            } else {
                0.0
            };
            let candidate = candidates
                .entry(ranked.node.id.clone())
                .or_insert_with(|| Candidate::new(ranked.node.clone()));
            candidate.bm25_rank = Some(index + 1);
            candidate.norm_bm25 = Some(normalized);
            candidate.lexical_seed = covers_graph_seed_terms(&candidate.node, &seed_terms);
        }
        for (index, ranked) in vector_ranked.into_iter().enumerate() {
            let candidate = candidates
                .entry(ranked.node.id.clone())
                .or_insert_with(|| Candidate::new(ranked.node.clone()));
            candidate.vector_rank = Some(index + 1);
            candidate.cosine = Some(ranked.cosine);
            candidate.embedding = Some(ranked.embedding);
        }
        for node in exact {
            let candidate = candidates
                .entry(node.id.clone())
                .or_insert_with(|| Candidate::new(node));
            candidate.exact = true;
        }

        let graph_candidates = self.walk_graph(query, quality, &mut candidates, &mut warnings)?;

        let mut fused = candidates
            .into_values()
            .filter_map(|mut candidate| {
                // Drop a signal that is too weak to trust before scoring, rather
                // than letting it veto the candidate. A vector that disagrees with
                // a verbatim lexical match is a missing opinion, not a refutation:
                // vetoing on it silently deleted records that contained the query
                // word, and the caller saw an empty answer with no warning.
                if candidate
                    .cosine
                    .is_some_and(|score| score < quality.min_cosine)
                {
                    candidate.cosine = None;
                    candidate.vector_rank = None;
                }
                if candidate
                    .norm_bm25
                    .is_some_and(|score| score < quality.min_bm25)
                {
                    candidate.norm_bm25 = None;
                    candidate.bm25_rank = None;
                }
                if let Some(rank) = candidate.bm25_rank {
                    candidate.score += 1.0 / (K_RRF + rank as f64);
                }
                if let Some(rank) = candidate.vector_rank {
                    candidate.score += 1.0 / (K_RRF + rank as f64);
                }
                if candidate.exact {
                    candidate.score += EXACT_BONUS;
                }
                // A neighbour ranks below anything the query matched directly:
                // it earned its place by association, not by answering.
                if let Some(rank) = candidate.graph_rank {
                    candidate.score += GRAPH_WEIGHT * candidate.graph_strength.unwrap_or(0.0)
                        / (K_RRF + rank as f64);
                }
                let signals = usize::from(candidate.bm25_rank.is_some())
                    + usize::from(candidate.vector_rank.is_some())
                    + usize::from(candidate.graph_rank.is_some())
                    + if candidate.exact { 2 } else { 0 };
                // Demanding two signals once vectors are available keeps only the
                // intersection of the lexical and semantic pools, and on a real
                // 10,590-node database that intersection is routinely empty. One
                // signal is enough when it is strong on its own.
                // A graph route is already corroboration by a confident seed
                // and one or more active edges. Requiring a lexical/vector hit
                // as well would make graph-only two-hop (and deeper) results
                // disappear whenever the vector index is available.
                let strong_alone = candidate
                    .norm_bm25
                    .is_some_and(|score| score >= quality.strong_bm25)
                    || candidate
                        .cosine
                        .is_some_and(|score| score >= quality.strong_cosine)
                    || candidate.graph_distance.is_some();
                let minimum_signals = if vector_search && !strong_alone {
                    MIN_SIGNALS
                } else {
                    1
                };
                if signals < minimum_signals && !candidate.exact {
                    return None;
                }
                if vector_search && !strong_alone && candidate.score < MIN_FINAL {
                    return None;
                }
                Some(candidate)
            })
            .collect::<Vec<_>>();
        fused.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.node.id.cmp(&right.node.id))
        });
        fused.truncate(MAX_CANDIDATES);

        let mut deduplicated = Vec::new();
        for candidate in fused {
            let duplicate = deduplicated.iter().any(|existing: &Candidate| {
                candidate.embedding.as_ref().is_some_and(|embedding| {
                    existing
                        .embedding
                        .as_ref()
                        .is_some_and(|other| cosine_similarity(embedding, other) > DEDUP_SIM)
                }) || normalized_text(&candidate.node) == normalized_text(&existing.node)
            });
            if !duplicate {
                deduplicated.push(candidate);
            }
        }

        let mut hits = Vec::new();
        let mut used_tokens = 0usize;
        for candidate in deduplicated {
            if candidate.node.token_len > budget_tokens {
                continue;
            }
            let Some(next_used) = used_tokens.checked_add(candidate.node.token_len) else {
                break;
            };
            // A hit that no longer fits is skipped, not the end of the list: lower-ranked
            // shorter hits still fill the rest of the budget.
            if next_used > budget_tokens {
                continue;
            }
            used_tokens = next_used;
            hits.push(HybridSearchHit {
                node: candidate.node,
                score: candidate.score,
                matched_bm25: candidate.bm25_rank.is_some(),
                matched_vector: candidate.vector_rank.is_some(),
                matched_exact: candidate.exact,
                graph_distance: candidate.graph_distance,
            });
        }
        if query.record_access && !hits.is_empty() {
            let node_ids = hits
                .iter()
                .map(|hit| hit.node.id.clone())
                .collect::<Vec<_>>();
            self.storage.record_node_accesses(
                &query.space_id,
                &node_ids,
                query.as_of.recorded_at,
            )?;
        }
        let graph_reached = hits
            .iter()
            .filter(|hit| hit.graph_distance.is_some())
            .count();
        if graph_candidates > 0 && graph_reached == 0 {
            warnings.push(format!(
                "graph walk found {graph_candidates} candidate(s), but none survived relevance, deduplication, and the token budget"
            ));
        }
        Ok(SearchOutput {
            hits,
            used_tokens,
            vector_search,
            graph_reached,
            warnings,
        })
    }
}

struct Bm25Ranked {
    node: Node,
    lexical: f64,
    weighted: f64,
}

struct Candidate {
    node: Node,
    embedding: Option<Vec<f32>>,
    bm25_rank: Option<usize>,
    norm_bm25: Option<f64>,
    lexical_seed: bool,
    vector_rank: Option<usize>,
    cosine: Option<f64>,
    exact: bool,
    graph_rank: Option<usize>,
    graph_distance: Option<u32>,
    graph_strength: Option<f64>,
    score: f64,
}

impl Candidate {
    /// How good a starting point this record is for walking the graph.
    ///
    /// Only records the query matched itself can be seeds; a record the walk
    /// already reached must not seed another walk, or one loose association
    /// drags in a whole neighbourhood.
    fn seed_strength(&self) -> f64 {
        if self.graph_rank.is_some() {
            return f64::MIN;
        }
        let lexical = self.norm_bm25.unwrap_or(0.0);
        let semantic = self.cosine.unwrap_or(0.0);
        let exact = if self.exact { 1.0 } else { 0.0 };
        lexical.max(semantic).max(exact)
    }

    /// Whether this record matched the query well enough to reason from.
    ///
    /// The same bar a lone signal has to clear to be returned at all: if it
    /// would not stand as an answer by itself, it has no business deciding
    /// which other records are relevant.
    fn is_confident_match(&self, quality: &QualitySettings) -> bool {
        if self.graph_rank.is_some() {
            return false;
        }
        self.exact
            || (self.lexical_seed
                && (self
                    .norm_bm25
                    .is_some_and(|score| score >= quality.strong_bm25)
                    || (self.bm25_rank.is_some()
                        && self
                            .cosine
                            .is_some_and(|score| score >= quality.strong_cosine))))
    }
}

impl Candidate {
    fn new(node: Node) -> Self {
        Self {
            node,
            embedding: None,
            bm25_rank: None,
            norm_bm25: None,
            lexical_seed: false,
            vector_rank: None,
            cosine: None,
            exact: false,
            graph_rank: None,
            graph_distance: None,
            graph_strength: None,
            score: 0.0,
        }
    }
}

/// Words this collection uses for a topic, learned from the records it reached.
///
/// A term earns a place by appearing in several of the retrieved records while
/// not being in the question already: one record's vocabulary is that record's,
/// but a word shared by three of them is how this corpus talks about the thing.
/// Rare enough to matter and common enough to be a topic, in other words.
fn expansion_terms(query: &str, hits: &[crate::SearchHit]) -> Vec<String> {
    if hits.is_empty() {
        return Vec::new();
    }
    let asked: HashSet<String> = query
        .split_whitespace()
        .map(normalized_term)
        .filter(|word| !word.is_empty())
        .collect();

    let mut documents_containing: HashMap<String, usize> = HashMap::new();
    for hit in hits {
        let mut seen = HashSet::new();
        let text = format!("{} {}", hit.node.label, hit.node.body);
        for word in text.split_whitespace() {
            let term = normalized_term(word);
            // Short words are mostly grammar, and anything already asked about
            // widens nothing.
            if term.chars().count() < EXPANSION_MIN_TERM_CHARS
                || asked.contains(&term)
                || crate::lexical::is_stop_word(&term)
            {
                continue;
            }
            if seen.insert(term.clone()) {
                *documents_containing.entry(term).or_default() += 1;
            }
        }
    }

    let required = EXPANSION_MIN_DOCUMENTS.min(hits.len());
    let mut ranked = documents_containing
        .into_iter()
        .filter(|(_, count)| *count >= required)
        .collect::<Vec<_>>();
    // Ties broken by length, not alphabetically: among words appearing equally
    // often, the longer ones carry the subject. Sorting by name instead handed
    // the expansion "came" and "from" while "sidechain" waited its turn.
    ranked.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.0.chars().count().cmp(&left.0.chars().count()))
            .then_with(|| left.0.cmp(&right.0))
    });
    // A single record's words are that record's, not the topic's, so borrow
    // fewer of them: enough to find its neighbours, not enough to change the
    // subject.
    let budget = if hits.len() == 1 {
        EXPANSION_MAX_TERMS.min(3)
    } else {
        EXPANSION_MAX_TERMS
    };
    ranked.truncate(budget);
    ranked.into_iter().map(|(term, _)| term).collect()
}

/// Lowercased letters and digits only, so punctuation does not split a word.
fn normalized_term(word: &str) -> String {
    word.chars()
        .filter(|character| character.is_alphanumeric() || *character == '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn node_boost(node: &Node, now: Timestamp) -> f64 {
    let current_salience = salience(
        SalienceInput {
            kind: node.kind,
            source_weight: node.source_weight,
            created_at: node.created_at,
            last_accessed: node.last_accessed,
            access_count: node.access_count,
            degree: node.degree,
        },
        now,
    );
    let since_access = now.saturating_sub(node.last_accessed).max(0) as f64;
    (0.2 + 0.8 * current_salience) * (-since_access / TAU_RECENCY).exp()
}

/// How much a record's length argues for or against it being the answer.
///
/// BM25 rewards a short document for containing the query word, which is right
/// for prose and wrong for the fragments a knowledge base accumulates: a heading
/// stored as its own record — "Почему так", three tokens — outscored the article
/// that actually explained the thing. Long records are diluted, and very short
/// ones cannot answer anything, so both ends are damped.
fn length_penalty(token_len: usize) -> f64 {
    let tokens = token_len.max(1) as f64;
    let dilution = (L_TARGET / tokens).sqrt().min(1.0);
    let substance = (tokens / L_SUBSTANTIAL).min(1.0);
    dilution * substance
}

fn fts_query(query: &str) -> String {
    // Borrowed corpus vocabulary remains literal: stemming it would compound
    // the widening of the user's own query and amplify unrelated neighbours.
    lexical_terms(query)
        .into_iter()
        .filter(|term| !crate::lexical::is_stop_word(term))
        .map(|term| format!("\"{term}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn fts_query_with_prefix(query: &str, prefix: bool) -> String {
    crate::lexical::bm25_query(query, prefix)
}

fn lexical_terms(text: &str) -> Vec<String> {
    crate::lexical::terms(text)
}

/// Terms a lexical match must cover before it is trusted to seed the graph.
///
/// A top BM25 hit is normalized to 1.0 even when it matched only one broad word
/// from a longer question. Requiring two substantive words keeps a query such
/// as "processor heat" from treating every processor utility as a reliable
/// route into its entire neighbourhood. One-word queries remain useful.
fn graph_seed_terms(query: &str) -> HashSet<String> {
    let all = lexical_terms(query).into_iter().collect::<HashSet<_>>();
    let substantive = all
        .iter()
        .filter(|term| term.chars().count() >= 3)
        .cloned()
        .collect::<HashSet<_>>();
    if substantive.is_empty() {
        all
    } else {
        substantive
    }
}

fn covers_graph_seed_terms(node: &Node, query_terms: &HashSet<String>) -> bool {
    if query_terms.is_empty() {
        return false;
    }
    let node_terms = lexical_terms(&format!("{} {}", node.label, node.body))
        .into_iter()
        .collect::<HashSet<_>>();
    query_terms.intersection(&node_terms).count() >= query_terms.len().min(2)
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
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

fn is_memory_record(node: &Node) -> bool {
    serde_json::from_str::<serde_json::Value>(&node.metadata_json)
        .ok()
        .and_then(|metadata| {
            metadata
                .get("brain_memory")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

fn normalized_text(node: &Node) -> String {
    format!("{} {}", node.label, node.body)
        .split_whitespace()
        .flat_map(str::chars)
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::EmbeddingError;
    use crate::{
        Agent, NewEdge, NewNode, NodeKind, Session, Space, SpaceKind, SqliteStorage, Storage,
    };

    const SPACE_ID: &str = "search-space";
    const AGENT_ID: &str = "search-agent";
    const SESSION_ID: &str = "search-session";

    struct KeywordEmbedder;

    impl Embedder for KeywordEmbedder {
        fn model_id(&self) -> &str {
            "test-keywords-256"
        }

        fn dimensions(&self) -> usize {
            256
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let mut vector = vec![0.0; 256];
            if text.to_lowercase().contains("compiler") {
                vector[0] = 1.0;
            } else {
                vector[1] = 1.0;
            }
            Ok(vector)
        }
    }

    /// One-hot by text length, so texts of different lengths never look like duplicates.
    struct LengthEmbedder;

    impl Embedder for LengthEmbedder {
        fn model_id(&self) -> &str {
            "test-length-256"
        }

        fn dimensions(&self) -> usize {
            256
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            // The query and the text mentioning "filler" share a direction, so the vector
            // signal can lift a longer text above shorter ones.
            let mut vector = vec![0.0; 256];
            let slot = if text == "compiler" || text.contains("filler") {
                0
            } else {
                1 + text.len() % 255
            };
            vector[slot] = 1.0;
            Ok(vector)
        }
    }

    struct GraphCollisionEmbedder;

    impl Embedder for GraphCollisionEmbedder {
        fn model_id(&self) -> &str {
            "test-graph-collision-256"
        }

        fn dimensions(&self) -> usize {
            256
        }

        fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let lower = text.to_lowercase();
            let mut vector = vec![0.0; 256];
            if lower.contains("anchor") {
                vector[0] = 1.0;
            } else if lower.contains("strong-vector") {
                vector[0] = 0.9;
                vector[1] = 0.435_889_9;
            } else if lower.contains("weak-vector") {
                // Cosine 0.30 with the query: admitted by precise (0.28), but
                // below its strong-alone threshold (0.50).
                vector[0] = 0.3;
                vector[1] = 0.953_939_2;
            } else {
                vector[1] = 1.0;
            }
            Ok(vector)
        }
    }

    fn setup() -> (SqliteStorage, Attribution) {
        let storage = SqliteStorage::open_in_memory().expect("open search database");
        storage
            .create_agent(&Agent {
                id: AGENT_ID.to_owned(),
                display_name: None,
                metadata_json: "{}".to_owned(),
                created_at: 1,
            })
            .expect("create agent");
        storage
            .create_space(&Space {
                id: SPACE_ID.to_owned(),
                kind: SpaceKind::Project,
                label: "Search".to_owned(),
                metadata_json: "{}".to_owned(),
                created_by_agent_id: AGENT_ID.to_owned(),
                created_at: 2,
            })
            .expect("create space");
        storage
            .create_session(&Session {
                id: SESSION_ID.to_owned(),
                space_id: SPACE_ID.to_owned(),
                agent_id: AGENT_ID.to_owned(),
                started_at: 3,
                ended_at: None,
                metadata_json: "{}".to_owned(),
            })
            .expect("create session");
        let attribution = Attribution {
            agent_id: AGENT_ID.to_owned(),
            session_id: SESSION_ID.to_owned(),
            recorded_at: 10,
        };
        (storage, attribution)
    }

    fn graph_node(id: &str, label: &str, body: &str, attribution: &Attribution) -> NewNode {
        NewNode {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            kind: NodeKind::Decision,
            label: label.to_owned(),
            body: body.to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from: 10,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 1.0,
            source_weight: 1.0,
        }
    }

    fn graph_edge(
        id: &str,
        from: &str,
        to: &str,
        valid_from: Timestamp,
        recorded_at: Timestamp,
        attribution: &Attribution,
    ) -> NewEdge {
        let mut attribution = attribution.clone();
        attribution.recorded_at = recorded_at;
        NewEdge {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            from_node_id: from.to_owned(),
            to_node_id: to.to_owned(),
            relation: "relates_to".to_owned(),
            weight: 0.9,
            confidence: 0.9,
            metadata_json: "{}".to_owned(),
            valid_from,
            valid_to: None,
            attribution,
            salience: 1.0,
        }
    }

    #[test]
    fn morphology_retrieves_without_exact_bonus_or_graph_authority() {
        let (storage, attribution) = setup();
        storage.insert_nodes(&[
            graph_node("forms", "Agent policy", "Агенты соблюдают правило: работать аккуратно, проверять изменения перед сохранением результата.", &attribution),
            graph_node("neighbour", "Unrelated", "Banana orchard irrigation calendar and rainfall measurements.", &attribution),
        ]).unwrap();
        storage
            .insert_edges(&[graph_edge(
                "edge",
                "forms",
                "neighbour",
                10,
                10,
                &attribution,
            )])
            .unwrap();
        let search = HybridSearch::without_vectors(&storage, "synthetic lexical test");
        let query = SearchQuery::new(SPACE_ID, "правила работы агентов", AsOf::new(20, 20));
        let output = search.search(&query, 12000).unwrap();
        let hit = output
            .hits
            .iter()
            .find(|hit| hit.node.id == "forms")
            .expect("cross-form BM25");
        assert!(hit.matched_bm25);
        assert!(!hit.matched_exact);
        assert!(!covers_graph_seed_terms(
            &hit.node,
            &graph_seed_terms(&query.text)
        ));
        assert_eq!(output.graph_reached, 0);
        assert!(!output.hits.iter().any(|hit| hit.node.id == "neighbour"));
        // The literal phrase still receives its exact signal.
        let exact = search
            .search(
                &SearchQuery::new(SPACE_ID, "Agent policy", AsOf::new(20, 20)),
                12000,
            )
            .unwrap();
        assert!(
            exact
                .hits
                .iter()
                .any(|hit| hit.node.id == "forms" && hit.matched_exact)
        );
    }

    #[test]
    fn stop_only_queries_do_not_use_exact_or_vector_fallbacks() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[graph_node(
                "grammar",
                "what",
                "что как где и на the and",
                &attribution,
            )])
            .unwrap();
        let search = HybridSearch::new(&storage, &KeywordEmbedder);
        for text in ["what", "что как где", "и на в а", "the and to", "!!!"] {
            let output = search
                .search(&SearchQuery::new(SPACE_ID, text, AsOf::new(20, 20)), 12000)
                .unwrap();
            assert!(output.hits.is_empty(), "{text}");
            assert!(!output.vector_search);
            assert_eq!(output.graph_reached, 0);
        }
    }

    #[test]
    fn search_never_exceeds_token_budget() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                NewNode {
                    id: "compiler-a".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Decision,
                    label: "Compiler architecture".to_owned(),
                    body: "Choose the compact compiler pipeline for local builds".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
                NewNode {
                    id: "compiler-b".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Code,
                    label: "Compiler cache".to_owned(),
                    body: "Compiler cache invalidation and artifact reuse details".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
            ])
            .expect("insert search nodes");

        let embedder = KeywordEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        for node_id in ["compiler-a", "compiler-b"] {
            let node = storage
                .node_as_of(SPACE_ID, node_id, AsOf::new(20, 20))
                .expect("load node")
                .expect("node exists");
            search
                .index_node(&node, attribution.clone(), 20)
                .expect("index node");
        }

        let output = search
            .search(
                &SearchQuery::new(SPACE_ID, "compiler", AsOf::new(20, 20)),
                200,
            )
            .expect("hybrid search");
        assert!(!output.hits.is_empty());
        assert!(output.used_tokens <= 200);
        assert_eq!(
            output.used_tokens,
            output
                .hits
                .iter()
                .map(|hit| hit.node.token_len)
                .sum::<usize>()
        );
    }

    #[test]
    fn a_hit_that_does_not_fit_leaves_room_for_shorter_ones_below_it() {
        let (storage, attribution) = setup();
        let node = |id: &str, label: &str, body: &str| NewNode {
            id: id.to_owned(),
            space_id: SPACE_ID.to_owned(),
            kind: NodeKind::Decision,
            label: label.to_owned(),
            body: body.to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from: 10,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 1.0,
            source_weight: 1.0,
        };
        storage
            .insert_nodes(&[
                node("top", "Compiler compiler", "compiler compiler compiler"),
                node(
                    "long",
                    "Compiler compiler notes",
                    "compiler compiler with several unrelated filler words",
                ),
                node("short", "Compiler", "compiler compiler"),
            ])
            .expect("insert search nodes");
        let embedder = LengthEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        for node_id in ["top", "long", "short"] {
            let stored = storage
                .node_as_of(SPACE_ID, node_id, AsOf::new(20, 20))
                .expect("load node")
                .expect("node exists");
            search
                .index_node(&stored, attribution.clone(), 20)
                .expect("index node");
        }
        let query = SearchQuery::new(SPACE_ID, "compiler", AsOf::new(20, 20));
        let ranked = search.search(&query, 100_000).expect("full ranking").hits;
        let lens = ranked
            .iter()
            .map(|hit| (hit.node.id.clone(), hit.node.token_len))
            .collect::<Vec<_>>();
        // The budget holds the second-ranked hit on its own, and the first with the third,
        // but not the first with the second: the second is skipped, the third still packed.
        assert_eq!(lens.len(), 3, "{lens:?}");
        let budget = lens[1].1.max(lens[0].1 + lens[2].1);
        assert!(
            budget < lens[0].1 + lens[1].1,
            "fixture must rank a longer hit second: {lens:?}"
        );

        let output = search.search(&query, budget).expect("hybrid search");
        let ids = output
            .hits
            .iter()
            .map(|hit| hit.node.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![lens[0].0.clone(), lens[2].0.clone()], "{lens:?}");
        assert_eq!(output.used_tokens, lens[0].1 + lens[2].1);
    }

    #[test]
    fn missing_model_degrades_to_memory_only_bm25_with_warning() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                NewNode {
                    id: "offline-memory".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Memory,
                    label: "Offline compiler fact".to_owned(),
                    body: "The compiler cache remains available without embeddings".to_owned(),
                    source: None,
                    metadata_json: r#"{"brain_memory":true,"kind":"fact"}"#.to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
                NewNode {
                    id: "offline-code".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Code,
                    label: "Offline compiler implementation".to_owned(),
                    body: "Compiler cache implementation detail".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution,
                    salience: 1.0,
                    source_weight: 1.0,
                },
            ])
            .expect("insert offline candidates");

        let search = HybridSearch::without_vectors(
            &storage,
            "embedding model is absent; using BM25 + exact matches only",
        );
        let output = search
            .search(
                &SearchQuery::new(SPACE_ID, "compiler cache", AsOf::new(20, 20)).memory_only(),
                500,
            )
            .expect("offline lexical search");

        assert_eq!(output.hits.len(), 1);
        assert_eq!(output.hits[0].node.id, "offline-memory");
        assert!(output.hits[0].matched_bm25);
        assert!(!output.vector_search);
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.contains("BM25 + exact"))
        );
        assert!(output.used_tokens <= 500);
    }

    #[test]
    fn expansion_prefers_subject_words_over_grammar() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[NewNode {
                id: "seed".to_owned(),
                space_id: SPACE_ID.to_owned(),
                kind: NodeKind::Error,
                label: "Encoder stutter".to_owned(),
                body: "The encoder stutter came from unstable sidechain timing".to_owned(),
                source: None,
                metadata_json: "{}".to_owned(),
                valid_from: 10,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 1.0,
                source_weight: 1.0,
            }])
            .expect("insert seed");
        let hits = storage
            .search_nodes_in_layers(
                SPACE_ID,
                &fts_query("stutter"),
                AsOf::new(20, 20),
                SearchLayers::HOT,
                10,
            )
            .expect("lexical search");
        assert_eq!(
            hits.len(),
            1,
            "the seed record must be found by its own word"
        );

        let terms = expansion_terms("stutter", &hits);
        assert!(
            terms.iter().any(|term| term == "sidechain"),
            "the subject word must be borrowed; got {terms:?}"
        );

        // And the borrowed word has to be searchable, or expansion changes
        // nothing however good the term selection is.
        let widened = storage
            .search_nodes_in_layers(
                SPACE_ID,
                &fts_query(&terms.join(" ")),
                AsOf::new(20, 20),
                SearchLayers::HOT,
                10,
            )
            .expect("widened search");
        assert!(
            !widened.is_empty(),
            "the widened query {terms:?} found nothing"
        );
    }

    /// Vocabulary shared by the records a thin query reached is what this
    /// collection calls the topic, and searching for it finds the rest.
    #[test]
    fn a_thin_query_borrows_the_words_the_corpus_uses() {
        let (storage, attribution) = setup();
        let mut nodes = vec![NewNode {
            id: "asked-about".to_owned(),
            space_id: SPACE_ID.to_owned(),
            kind: NodeKind::Error,
            label: "Encoder stutter".to_owned(),
            body: "The encoder stutter came from unstable sidechain timing".to_owned(),
            source: None,
            metadata_json: "{}".to_owned(),
            valid_from: 10,
            valid_to: None,
            attribution: attribution.clone(),
            salience: 1.0,
            source_weight: 1.0,
        }];
        // Three records share "sidechain", none of them mention "stutter".
        for index in 0..3 {
            nodes.push(NewNode {
                id: format!("shares-vocabulary-{index}"),
                space_id: SPACE_ID.to_owned(),
                kind: NodeKind::Decision,
                label: format!("Sidechain fix {index}"),
                body: format!(
                    "Pinning sidechain sample counts removed the flakiness, attempt {index}"
                ),
                source: None,
                metadata_json: "{}".to_owned(),
                valid_from: 10,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 1.0,
                source_weight: 1.0,
            });
        }
        // Records on other subjects, so that "sidechain" is rare in the corpus.
        // BM25 scores a term appearing in every document at almost nothing —
        // its inverse document frequency is zero — and without these the
        // borrowed word would be worthless by construction rather than by any
        // property of the code.
        for index in 0..6 {
            nodes.push(NewNode {
                id: format!("unrelated-{index}"),
                space_id: SPACE_ID.to_owned(),
                kind: NodeKind::Document,
                label: format!("Release checklist {index}"),
                body: format!(
                    "Tagging, changelog and package upload for release {index}, unrelated to audio"
                ),
                source: None,
                metadata_json: "{}".to_owned(),
                valid_from: 10,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 1.0,
                source_weight: 1.0,
            });
        }
        storage.insert_nodes(&nodes).expect("insert nodes");

        let by_shared_word = storage
            .search_nodes_in_layers(
                SPACE_ID,
                &fts_query("sidechain"),
                AsOf::new(20, 20),
                SearchLayers::HOT,
                10,
            )
            .expect("search by shared word");
        assert_eq!(
            by_shared_word.len(),
            4,
            "the four audio records share the word; found {:?}",
            by_shared_word
                .iter()
                .map(|hit| &hit.node.id)
                .collect::<Vec<_>>()
        );

        let search = HybridSearch::without_vectors(&storage, "no model in this test");
        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "stutter", AsOf::new(20, 20)),
                &crate::profile::Profile::Max.settings(),
            )
            .expect("search");

        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.contains("widened")),
            "the widening has to happen and be reported; warnings were {:?}",
            output.warnings
        );
        assert_eq!(
            output.hits[0].node.id, "asked-about",
            "the record that actually matched the words must still rank first"
        );
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.node.id.starts_with("shares-vocabulary")),
            "borrowed-vocabulary records must survive to the answer; hits {:?}, used {} of {} tokens",
            output
                .hits
                .iter()
                .map(|hit| (&hit.node.id, hit.node.token_len))
                .collect::<Vec<_>>(),
            output.used_tokens,
            crate::profile::Profile::Max.settings().budget_tokens
        );
        // Measured on the live 10,590-record database as well: "ffmpeg
        // недетерминизм" went from 6 hits to 21, and the added records were the
        // treatment and the diagnosis for that exact problem.
    }

    /// The graph carries the association that static embeddings cannot: a
    /// record sharing no words with the query is reachable through what the
    /// query did match. And it only walks from a confident match — following a
    /// weak one returns unrelated records with the confidence of a real answer.
    #[test]
    fn the_graph_walk_reaches_what_words_cannot_but_only_from_a_sure_match() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                NewNode {
                    id: "compiler-note".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Decision,
                    label: "Compiler pipeline".to_owned(),
                    body: "The compiler pipeline was rebuilt for local builds".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
                NewNode {
                    id: "unrelated-words".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Error,
                    label: "Thermal shutdown".to_owned(),
                    body: "The machine shut itself down under sustained load".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
            ])
            .expect("insert nodes");
        storage
            .insert_edge(&NewEdge {
                id: "compiler-causes-thermal".to_owned(),
                space_id: SPACE_ID.to_owned(),
                from_node_id: "compiler-note".to_owned(),
                to_node_id: "unrelated-words".to_owned(),
                relation: "relates_to".to_owned(),
                weight: 0.9,
                confidence: 0.9,
                metadata_json: "{}".to_owned(),
                valid_from: 10,
                valid_to: None,
                attribution: attribution.clone(),
                salience: 1.0,
            })
            .expect("insert edge");
        storage.rebuild_paths(SPACE_ID).expect("build paths");

        let search = HybridSearch::without_vectors(&storage, "no model in this test");
        let walked = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "compiler", AsOf::new(20, 20)),
                &crate::profile::Profile::Precise.settings(),
            )
            .expect("search with graph");
        assert!(
            walked
                .hits
                .iter()
                .any(|hit| hit.node.id == "unrelated-words" && hit.graph_distance == Some(1)),
            "the neighbour of a confident match must be reachable; got {:?}",
            walked
                .hits
                .iter()
                .map(|hit| &hit.node.id)
                .collect::<Vec<_>>()
        );

        // The depth setting has to actually govern the walk. It sat in the
        // profile, was printed to the user, and had a slider in the settings
        // window for months while nothing in search ever read it.
        let no_walk = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "compiler", AsOf::new(20, 20)),
                &crate::profile::Profile::Fast.settings(),
            )
            .expect("search at zero hops");
        assert_eq!(
            crate::profile::Profile::Fast.settings().graph_hops,
            0,
            "this test is only meaningful while fast means no walking"
        );
        assert_eq!(
            no_walk
                .hits
                .iter()
                .filter(|hit| hit.graph_distance.is_some())
                .count(),
            0,
            "zero hops must mean no neighbours"
        );
        assert_eq!(no_walk.graph_reached, 0);
    }

    #[test]
    fn weak_vector_leaf_still_receives_graph_evidence() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "anchor-node",
                    "Anchor record",
                    "anchor evidence for the graph walk",
                    &attribution,
                ),
                graph_node(
                    "weak-leaf",
                    "Weak-vector leaf",
                    "weak-vector record with no query word",
                    &attribution,
                ),
            ])
            .expect("insert collision nodes");
        storage
            .insert_edge(&graph_edge(
                "anchor-leaf",
                "anchor-node",
                "weak-leaf",
                10,
                10,
                &attribution,
            ))
            .expect("insert leaf edge");

        let embedder = GraphCollisionEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        for node_id in ["anchor-node", "weak-leaf"] {
            let node = storage
                .node_as_of(SPACE_ID, node_id, AsOf::new(20, 20))
                .expect("load collision node")
                .expect("collision node exists");
            search
                .index_node(&node, attribution.clone(), 20)
                .expect("index collision node");
        }

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("search collision graph");
        let leaf = output
            .hits
            .iter()
            .find(|hit| hit.node.id == "weak-leaf")
            .expect("weak vector leaf must survive");
        assert!(
            leaf.matched_vector,
            "the weak vector signal must be retained"
        );
        assert_eq!(leaf.graph_distance, Some(1));
        assert_eq!(
            output.graph_reached,
            output
                .hits
                .iter()
                .filter(|hit| hit.graph_distance.is_some())
                .count()
        );
    }

    #[test]
    fn two_hop_graph_only_survives_an_active_vector_pool() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "anchor-node",
                    "Anchor record",
                    "anchor evidence for the graph walk",
                    &attribution,
                ),
                graph_node(
                    "bridge-node",
                    "Bridge",
                    "weak-vector bridge without the query term",
                    &attribution,
                ),
                graph_node(
                    "two-hop-target",
                    "Remote answer",
                    "the graph-only record the second hop must retrieve",
                    &attribution,
                ),
            ])
            .expect("insert two-hop nodes");
        storage
            .insert_edges(&[
                graph_edge(
                    "anchor-bridge",
                    "anchor-node",
                    "bridge-node",
                    10,
                    10,
                    &attribution,
                ),
                graph_edge(
                    "bridge-target",
                    "bridge-node",
                    "two-hop-target",
                    10,
                    10,
                    &attribution,
                ),
            ])
            .expect("insert two-hop edges");

        let embedder = GraphCollisionEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        for node_id in ["anchor-node", "bridge-node"] {
            let node = storage
                .node_as_of(SPACE_ID, node_id, AsOf::new(20, 20))
                .expect("load indexed node")
                .expect("indexed node exists");
            search
                .index_node(&node, attribution.clone(), 20)
                .expect("index graph node");
        }

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("search two hops with vectors");
        assert!(output.vector_search);
        assert!(
            output
                .hits
                .iter()
                .any(|hit| { hit.node.id == "two-hop-target" && hit.graph_distance == Some(2) })
        );
        assert_eq!(
            output.graph_reached,
            output
                .hits
                .iter()
                .filter(|hit| hit.graph_distance.is_some())
                .count()
        );

        let mut tiny_budget = Profile::Precise.settings();
        tiny_budget.budget_tokens = 1;
        let excluded = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(20, 20)),
                &tiny_budget,
            )
            .expect("search with excluding budget");
        assert!(excluded.hits.is_empty());
        assert_eq!(excluded.graph_reached, 0);
        assert!(excluded.warnings.iter().any(|warning| {
            warning.contains("graph walk found") && warning.contains("none survived")
        }));
    }

    #[test]
    fn graph_edges_respect_the_query_as_of_coordinate() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "past-anchor",
                    "Past anchor",
                    "anchor known before the edge existed",
                    &attribution,
                ),
                graph_node(
                    "future-neighbor",
                    "Future neighbor",
                    "a node linked only in the future",
                    &attribution,
                ),
            ])
            .expect("insert temporal graph nodes");
        storage
            .insert_edge(&graph_edge(
                "future-edge",
                "past-anchor",
                "future-neighbor",
                200,
                200,
                &attribution,
            ))
            .expect("insert future edge");
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let before = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(100, 100)),
                &Profile::Balanced.settings(),
            )
            .expect("search before edge");
        assert!(
            !before
                .hits
                .iter()
                .any(|hit| hit.node.id == "future-neighbor")
        );

        let after = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(300, 300)),
                &Profile::Balanced.settings(),
            )
            .expect("search after edge");
        assert!(
            after
                .hits
                .iter()
                .any(|hit| { hit.node.id == "future-neighbor" && hit.graph_distance == Some(1) })
        );
    }

    #[test]
    fn four_hops_work_in_both_edge_directions_without_materialized_paths() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "chain-a",
                    "First record",
                    "start of the chain",
                    &attribution,
                ),
                graph_node(
                    "chain-b",
                    "Second record",
                    "second chain link",
                    &attribution,
                ),
                graph_node("chain-c", "Third record", "third chain link", &attribution),
                graph_node(
                    "chain-d",
                    "Fourth record",
                    "fourth chain link",
                    &attribution,
                ),
                graph_node(
                    "chain-e",
                    "Reverse anchor",
                    "reverse-anchor at the end of the chain",
                    &attribution,
                ),
            ])
            .expect("insert chain nodes");
        storage
            .insert_edges(&[
                graph_edge("chain-ab", "chain-a", "chain-b", 10, 10, &attribution),
                graph_edge("chain-bc", "chain-b", "chain-c", 10, 10, &attribution),
                graph_edge("chain-cd", "chain-c", "chain-d", 10, 10, &attribution),
                graph_edge("chain-de", "chain-d", "chain-e", 10, 10, &attribution),
            ])
            .expect("insert chain edges");
        assert!(
            storage
                .materialized_paths_from(SPACE_ID, "chain-e", 20)
                .expect("read paths from the reverse seed")
                .is_empty(),
            "this must prove the reversed walk does not depend on a materialized path"
        );

        let mut quality = Profile::Max.settings();
        quality.graph_hops = 4;
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");
        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "reverse anchor", AsOf::new(20, 20)),
                &quality,
            )
            .expect("walk the reversed chain");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| { hit.node.id == "chain-a" && hit.graph_distance == Some(4) }),
            "four-hop target missing: {output:#?}"
        );
        assert!(
            !output
                .warnings
                .iter()
                .any(|warning| warning.contains("reindex --paths"))
        );
    }

    #[test]
    fn zero_confident_seed_explains_why_the_graph_did_not_start() {
        let (storage, attribution) = setup();
        storage
            .insert_node(&graph_node(
                "weak-only",
                "Weak-vector candidate",
                "weak-vector record with unrelated vocabulary",
                &attribution,
            ))
            .expect("insert weak candidate");
        let embedder = GraphCollisionEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        let node = storage
            .node_as_of(SPACE_ID, "weak-only", AsOf::new(20, 20))
            .expect("load weak node")
            .expect("weak node exists");
        search
            .index_node(&node, attribution, 20)
            .expect("index weak node");

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("search without confident seed");
        assert_eq!(output.graph_reached, 0);
        assert!(
            output.warnings.iter().any(|warning| {
                warning.contains("graph walk skipped") && warning.contains("strong enough")
            }),
            "missing seed diagnostic: {output:#?}"
        );
    }

    #[test]
    fn vector_only_match_cannot_seed_unrelated_graph_results() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "vector-only",
                    "Strong-vector candidate",
                    "strong-vector record with unrelated vocabulary",
                    &attribution,
                ),
                graph_node(
                    "vector-noise",
                    "Associated helper",
                    "a record that the query never matched",
                    &attribution,
                ),
            ])
            .expect("insert vector-only graph");
        storage
            .insert_edge(&graph_edge(
                "vector-noise-edge",
                "vector-only",
                "vector-noise",
                10,
                10,
                &attribution,
            ))
            .expect("insert vector-only edge");
        let embedder = GraphCollisionEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        let node = storage
            .node_as_of(SPACE_ID, "vector-only", AsOf::new(20, 20))
            .expect("load vector-only node")
            .expect("vector-only node exists");
        search
            .index_node(&node, attribution, 20)
            .expect("index vector-only node");

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "anchor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("search from vector-only evidence");
        assert!(output.hits.iter().any(|hit| {
            hit.node.id == "vector-only" && hit.matched_vector && hit.graph_distance.is_none()
        }));
        assert!(!output.hits.iter().any(|hit| hit.node.id == "vector-noise"));
        assert_eq!(output.graph_reached, 0);
        assert!(output.warnings.iter().any(|warning| {
            warning.contains("graph walk skipped") && warning.contains("strong enough")
        }));
    }

    #[test]
    fn one_word_from_a_multiword_query_cannot_seed_the_graph() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "processor-tool",
                    "Processor utility",
                    "processor telemetry and utilization counters",
                    &attribution,
                ),
                graph_node(
                    "unrelated-neighbor",
                    "Unrelated helper",
                    "an implementation detail reached only by association",
                    &attribution,
                ),
            ])
            .expect("insert partial-match graph");
        storage
            .insert_edge(&graph_edge(
                "processor-helper",
                "processor-tool",
                "unrelated-neighbor",
                10,
                10,
                &attribution,
            ))
            .expect("insert partial-match edge");
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "heat processor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("search from a partial lexical match");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.node.id == "processor-tool")
        );
        assert!(
            !output
                .hits
                .iter()
                .any(|hit| { hit.node.id == "unrelated-neighbor" || hit.graph_distance.is_some() })
        );
        assert_eq!(output.graph_reached, 0);
        assert!(output.warnings.iter().any(|warning| {
            warning.contains("graph walk skipped") && warning.contains("strong enough")
        }));
    }

    #[test]
    fn zero_weight_edge_is_not_graph_evidence() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                graph_node(
                    "zero-anchor",
                    "Zero anchor",
                    "anchor record for a zero-weight edge",
                    &attribution,
                ),
                graph_node(
                    "zero-leaf",
                    "Disconnected leaf",
                    "unrelated record behind no usable evidence",
                    &attribution,
                ),
            ])
            .expect("insert zero-weight nodes");
        let mut edge = graph_edge(
            "zero-edge",
            "zero-anchor",
            "zero-leaf",
            10,
            10,
            &attribution,
        );
        edge.weight = 0.0;
        storage.insert_edge(&edge).expect("insert zero-weight edge");
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "zero anchor", AsOf::new(20, 20)),
                &Profile::Balanced.settings(),
            )
            .expect("search across a zero-weight edge");
        // A zero-weight edge must not carry the record in. Reaching it by any
        // other route — shared vocabulary, for instance — is a different claim
        // and not what this test is about, so the assertion is about the graph
        // specifically rather than about the record's absence.
        assert!(
            !output
                .hits
                .iter()
                .any(|hit| hit.node.id == "zero-leaf" && hit.graph_distance.is_some())
        );
        assert_eq!(output.graph_reached, 0);
        assert!(
            output
                .warnings
                .iter()
                .any(|warning| warning.contains("no eligible active connection"))
        );
    }

    #[test]
    fn inactive_intermediate_cannot_bridge_two_active_nodes() {
        let (storage, attribution) = setup();
        let mut future_bridge = graph_node(
            "future-bridge",
            "Future bridge",
            "middle of a route that does not exist yet",
            &attribution,
        );
        future_bridge.valid_from = 200;
        storage
            .insert_nodes(&[
                graph_node(
                    "temporal-anchor",
                    "Temporal anchor",
                    "anchor record before the bridge exists",
                    &attribution,
                ),
                future_bridge,
                graph_node(
                    "temporal-target",
                    "Temporal target",
                    "remote answer beyond the future bridge",
                    &attribution,
                ),
            ])
            .expect("insert temporal route nodes");
        storage
            .insert_edges(&[
                graph_edge(
                    "temporal-first",
                    "temporal-anchor",
                    "future-bridge",
                    10,
                    10,
                    &attribution,
                ),
                graph_edge(
                    "temporal-second",
                    "future-bridge",
                    "temporal-target",
                    10,
                    10,
                    &attribution,
                ),
            ])
            .expect("insert temporal route edges");
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let before = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "temporal anchor", AsOf::new(100, 100)),
                &Profile::Precise.settings(),
            )
            .expect("search before the intermediate node exists");
        assert!(
            !before
                .hits
                .iter()
                .any(|hit| { hit.node.id == "future-bridge" || hit.node.id == "temporal-target" })
        );

        let after = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "temporal anchor", AsOf::new(300, 300)),
                &Profile::Precise.settings(),
            )
            .expect("search after the intermediate node exists");
        assert!(
            after
                .hits
                .iter()
                .any(|hit| { hit.node.id == "temporal-target" && hit.graph_distance == Some(2) })
        );
    }

    #[test]
    fn hot_only_graph_walk_does_not_leak_a_cooled_neighbor() {
        let (storage, attribution) = setup();
        let anchor = graph_node(
            "layer-anchor",
            "Layer anchor",
            "layer anchor that remains a durable decision",
            &attribution,
        );
        let mut archived = graph_node(
            "layer-archived",
            "Archived session",
            "routine interaction that has cooled out of the hot layer",
            &attribution,
        );
        archived.kind = NodeKind::Session;
        storage
            .insert_nodes(&[anchor, archived])
            .expect("insert differently retained nodes");
        storage
            .insert_edge(&graph_edge(
                "layer-edge",
                "layer-anchor",
                "layer-archived",
                10,
                10,
                &attribution,
            ))
            .expect("insert layer edge");
        let now = 10 + 100 * DAY;
        storage
            .maintenance(now, 20, false)
            .expect("cool the session node");
        let archived = storage
            .node_as_of(SPACE_ID, "layer-archived", AsOf::new(now, now))
            .expect("load cooled node")
            .expect("cooled node exists");
        assert_ne!(archived.salience_layer, crate::SalienceLayer::Hot);
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let hot_only = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "layer anchor", AsOf::new(now, now)),
                &Profile::Balanced.settings(),
            )
            .expect("walk only the hot layer");
        assert!(
            !hot_only
                .hits
                .iter()
                .any(|hit| hit.node.id == "layer-archived")
        );
        assert!(hot_only.warnings.iter().any(|warning| {
            warning.contains("no eligible active connection") && warning.contains("time and scope")
        }));

        let all_layers = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "layer anchor", AsOf::new(now, now))
                    .with_layers(SearchLayers::ALL),
                &Profile::Balanced.settings(),
            )
            .expect("walk every layer");
        assert!(
            all_layers
                .hits
                .iter()
                .any(|hit| { hit.node.id == "layer-archived" && hit.graph_distance == Some(1) })
        );
    }

    #[test]
    fn memory_only_graph_walk_uses_only_memory_records() {
        let (storage, attribution) = setup();
        let mut anchor = graph_node(
            "memory-anchor",
            "Memory anchor",
            "memory anchor written through the memory API",
            &attribution,
        );
        anchor.metadata_json = r#"{"brain_memory":true,"kind":"fact"}"#.to_owned();
        let ordinary = graph_node(
            "ordinary-neighbor",
            "Ordinary indexed node",
            "code or documentation outside the memory API",
            &attribution,
        );
        let mut remembered = graph_node(
            "remembered-neighbor",
            "Remembered neighbor",
            "related fact written through the memory API",
            &attribution,
        );
        remembered.metadata_json = r#"{"brain_memory":true,"kind":"fact"}"#.to_owned();
        storage
            .insert_nodes(&[anchor, ordinary, remembered])
            .expect("insert mixed graph records");
        storage
            .insert_edges(&[
                graph_edge(
                    "ordinary-memory-edge",
                    "memory-anchor",
                    "ordinary-neighbor",
                    10,
                    10,
                    &attribution,
                ),
                graph_edge(
                    "remembered-memory-edge",
                    "memory-anchor",
                    "remembered-neighbor",
                    10,
                    10,
                    &attribution,
                ),
            ])
            .expect("insert mixed graph edges");
        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");

        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "memory anchor", AsOf::new(20, 20)).memory_only(),
                &Profile::Balanced.settings(),
            )
            .expect("walk memory-only graph");
        assert!(
            !output
                .hits
                .iter()
                .any(|hit| hit.node.id == "ordinary-neighbor")
        );
        assert!(
            output.hits.iter().any(|hit| {
                hit.node.id == "remembered-neighbor" && hit.graph_distance == Some(1)
            })
        );
    }

    #[test]
    fn bounded_frontier_is_reported_instead_of_looking_exhaustive() {
        let (storage, attribution) = setup();
        let mut nodes = vec![graph_node(
            "beam-anchor",
            "Beam anchor",
            "anchor record for a wide graph",
            &attribution,
        )];
        for index in 0..=GRAPH_FRONTIER {
            nodes.push(graph_node(
                &format!("beam-bridge-{index:02}"),
                &format!("Bridge {index:02}"),
                "one of many equally weighted branches",
                &attribution,
            ));
        }
        nodes.push(graph_node(
            "beam-target",
            "Remote target",
            "answer hidden behind the branch outside the frontier",
            &attribution,
        ));
        storage
            .insert_nodes(&nodes)
            .expect("insert wide graph nodes");

        let mut edges = (0..=GRAPH_FRONTIER)
            .map(|index| {
                graph_edge(
                    &format!("beam-edge-{index:02}"),
                    "beam-anchor",
                    &format!("beam-bridge-{index:02}"),
                    10,
                    10,
                    &attribution,
                )
            })
            .collect::<Vec<_>>();
        edges.push(graph_edge(
            "beam-target-edge",
            &format!("beam-bridge-{GRAPH_FRONTIER:02}"),
            "beam-target",
            10,
            10,
            &attribution,
        ));
        storage
            .insert_edges(&edges)
            .expect("insert wide graph edges");

        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");
        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "beam anchor", AsOf::new(20, 20)),
                &Profile::Precise.settings(),
            )
            .expect("walk a bounded frontier");
        assert!(!output.hits.iter().any(|hit| hit.node.id == "beam-target"));
        assert!(
            output.warnings.iter().any(|warning| {
                warning.contains("bounded") && warning.contains("not exhaustive")
            })
        );
    }

    #[test]
    fn bounded_seed_set_is_reported_instead_of_looking_exhaustive() {
        let (storage, attribution) = setup();
        let nodes = (0..=GRAPH_SEEDS)
            .map(|index| {
                graph_node(
                    &format!("seed-{index:02}"),
                    "Shared exact anchor",
                    &format!("direct match number {index:02}"),
                    &attribution,
                )
            })
            .chain(std::iter::once(graph_node(
                "seed-target",
                "Remote seed target",
                "answer behind the direct match outside the retained seed set",
                &attribution,
            )))
            .collect::<Vec<_>>();
        storage.insert_nodes(&nodes).expect("insert seed-cap nodes");
        storage
            .insert_edge(&graph_edge(
                "seed-cap-edge",
                &format!("seed-{GRAPH_SEEDS:02}"),
                "seed-target",
                10,
                10,
                &attribution,
            ))
            .expect("insert edge outside the retained seed set");

        let search = HybridSearch::without_vectors(&storage, "test uses lexical search only");
        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "Shared exact anchor", AsOf::new(20, 20)),
                &Profile::Balanced.settings(),
            )
            .expect("walk a bounded seed set");
        assert!(!output.hits.iter().any(|hit| hit.node.id == "seed-target"));
        assert!(output.warnings.iter().any(|warning| {
            warning.contains("bounded to 12 seeds") && warning.contains("not exhaustive")
        }));
    }

    /// A record containing the query word verbatim must survive even when the
    /// vector index disagrees with it. Demanding two signals kept only the
    /// intersection of the two pools, and on a real 10,590-node database that
    /// intersection was empty often enough to return nothing at all.
    #[test]
    fn a_strong_lexical_hit_survives_without_a_vector_agreeing() {
        let (storage, attribution) = setup();
        storage
            .insert_nodes(&[
                NewNode {
                    id: "compiler-note".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Decision,
                    label: "Compiler pipeline".to_owned(),
                    body: "The compiler pipeline was rebuilt for local builds".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
                NewNode {
                    id: "overheating-note".to_owned(),
                    space_id: SPACE_ID.to_owned(),
                    kind: NodeKind::Error,
                    label: "Overheating".to_owned(),
                    body: "Overheating traced to a runaway indexing job".to_owned(),
                    source: None,
                    metadata_json: "{}".to_owned(),
                    valid_from: 10,
                    valid_to: None,
                    attribution: attribution.clone(),
                    salience: 1.0,
                    source_weight: 1.0,
                },
            ])
            .expect("insert nodes");

        let embedder = KeywordEmbedder;
        let search = HybridSearch::new(&storage, &embedder);
        for node_id in ["compiler-note", "overheating-note"] {
            let node = storage
                .node_as_of(SPACE_ID, node_id, AsOf::new(20, 20))
                .expect("load node")
                .expect("node exists");
            search
                .index_node(&node, attribution.clone(), 20)
                .expect("index node");
        }

        // The query embeds towards "compiler", so the overheating record is a
        // pure lexical hit: the vector index actively disagrees with it.
        let output = search
            .search_with(
                &SearchQuery::new(SPACE_ID, "compiler overheating", AsOf::new(20, 20)),
                &crate::profile::Profile::Balanced.settings(),
            )
            .expect("hybrid search");

        assert!(output.vector_search, "the vector pool must be in play");
        assert!(
            output
                .hits
                .iter()
                .any(|hit| hit.node.id == "overheating-note"),
            "a verbatim lexical match must not need a vector to confirm it; got {:?}",
            output
                .hits
                .iter()
                .map(|hit| &hit.node.id)
                .collect::<Vec<_>>()
        );
    }
}
