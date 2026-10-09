//! Salience decay and persisted maintenance scheduling.

use crate::{NodeKind, SalienceLayer, Timestamp};

/// One day in the timestamp unit used by the storage model.
pub const DAY: Timestamp = 86_400;
/// Hot-layer threshold from the product search design.
pub const THETA_HOT: f64 = 0.70;
/// Warm-layer threshold from the product search design.
pub const THETA_WARM: f64 = 0.35;

const ALPHA: f64 = 0.3;
const BETA: f64 = 0.5;
const A_MAX: f64 = 3.0;
const GAMMA: f64 = 0.18;
const H_MAX: f64 = 0.40;
const T_STRUCT: f64 = 365.0 * DAY as f64;
const HUB_DEGREE: i64 = 100;
const HUB_MAX_AGE: Timestamp = 2 * 365 * DAY;
const COLD_RECALC_INTERVAL: Timestamp = 7 * DAY;
const MAX_LOOKAHEAD: Timestamp = 365 * DAY;
const TIME_EPS: Timestamp = 60 * 60;

/// Inputs required by the decay formula, independent of a storage backend.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SalienceInput {
    /// Node category controlling the base decay period.
    pub kind: NodeKind,
    /// Source reliability before the formula's 0.2 floor is applied.
    pub source_weight: f64,
    /// Node creation time.
    pub created_at: Timestamp,
    /// Most recent access time.
    pub last_accessed: Timestamp,
    /// Number of recorded accesses.
    pub access_count: i64,
    /// Number of incident edges.
    pub degree: i64,
}

/// Returns the salience described in `algorithms-salience-search-paths.md`.
#[must_use]
pub fn salience(input: SalienceInput, now: Timestamp) -> f64 {
    let weight = input.source_weight.clamp(0.2, 1.0);
    let age = now.saturating_sub(input.created_at).max(0) as f64;
    let last_relevant_access = input.last_accessed.max(input.created_at);
    let since_access = now.saturating_sub(last_relevant_access).max(0) as f64;
    let access_count = input.access_count.max(0) as f64;
    let degree = input.degree.max(0) as f64;

    let half_life =
        half_life(input.kind) * (1.0 + BETA * access_count.ln_1p()) * (0.5 + 0.5 * weight);
    let access_amplifier = (1.0 + ALPHA * access_count.ln_1p()).min(A_MAX);
    let recency = weight * access_amplifier * (-since_access / half_life).exp();
    let structural = (GAMMA * degree.ln_1p().sqrt()).min(H_MAX) * (-age / T_STRUCT).exp();

    let mut score = recency + structural;
    if input.degree > HUB_DEGREE && now.saturating_sub(input.created_at) < HUB_MAX_AGE {
        score = score.max(THETA_WARM + 0.000_1);
    }
    score.clamp(0.0, 1.0)
}

/// Maps a salience score to its persisted search layer.
#[must_use]
pub fn layer_of(score: f64) -> SalienceLayer {
    if score >= THETA_HOT {
        SalienceLayer::Hot
    } else if score >= THETA_WARM {
        SalienceLayer::Warm
    } else {
        SalienceLayer::Cold
    }
}

/// Computes the next persisted maintenance deadline for one node.
#[must_use]
pub fn next_recalculation_at(input: SalienceInput, now: Timestamp) -> Timestamp {
    let score = salience(input, now);
    let target = match layer_of(score) {
        SalienceLayer::Hot => Some(THETA_WARM),
        SalienceLayer::Warm => Some(THETA_WARM * 0.5),
        SalienceLayer::Cold => None,
    };
    let Some(target) = target else {
        return now.saturating_add(COLD_RECALC_INTERVAL);
    };

    now.saturating_add(binary_search_crossing(input, now, target))
}

fn binary_search_crossing(input: SalienceInput, now: Timestamp, target: f64) -> Timestamp {
    let mut low = 0;
    let mut high = MAX_LOOKAHEAD;
    while high - low > TIME_EPS {
        let middle = low + (high - low) / 2;
        if salience(input, now.saturating_add(middle)) > target {
            low = middle;
        } else {
            high = middle;
        }
    }
    high
}

fn half_life(kind: NodeKind) -> f64 {
    let days = match kind {
        NodeKind::Decision | NodeKind::Error => 365,
        NodeKind::Code | NodeKind::Document | NodeKind::Concept | NodeKind::Entity => 90,
        NodeKind::Memory => 30,
        NodeKind::Session => 7,
    };
    f64::from(days) * DAY as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: NodeKind) -> SalienceInput {
        SalienceInput {
            kind,
            source_weight: 1.0,
            created_at: 0,
            last_accessed: 0,
            access_count: 0,
            degree: 0,
        }
    }

    #[test]
    fn decisions_outlive_sessions_after_one_hundred_days() {
        let now = 100 * DAY;
        assert_eq!(
            layer_of(salience(input(NodeKind::Decision), now)),
            SalienceLayer::Hot
        );
        assert_ne!(
            layer_of(salience(input(NodeKind::Session), now)),
            SalienceLayer::Hot
        );
    }
}
