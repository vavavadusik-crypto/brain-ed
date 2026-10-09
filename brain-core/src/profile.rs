//! Quality profiles: the user's chosen point on the cost/accuracy curve.
//!
//! Every knob here was hard-coded before. Exposing them is the whole point of
//! the product: spending more tokens and more time must buy measurably better
//! answers, and the user decides how much to spend.

use std::fmt;
use std::str::FromStr;

/// A named point on the cost/accuracy curve.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Profile {
    /// Cheapest: shallow candidate pools, no graph expansion.
    Fast,
    /// Default: the values the product shipped with.
    #[default]
    Balanced,
    /// Wider pools, one more graph hop, larger answer budget.
    Precise,
    /// Everything on. For questions worth waiting for.
    Max,
}

/// Concrete search settings derived from a profile or set by hand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QualitySettings {
    /// Candidates pulled from each signal before fusion.
    pub candidates_per_signal: usize,
    /// How far to walk the graph from a matched node.
    pub graph_hops: u32,
    /// Hard ceiling on the answer, in tokens.
    pub budget_tokens: usize,
    /// Minimum cosine similarity for a vector hit to count.
    pub min_cosine: f64,
    /// Minimum normalized BM25 score for a lexical hit to count.
    pub min_bm25: f64,
    /// Normalized BM25 score above which a lexical hit stands on its own,
    /// without a vector hit agreeing with it.
    pub strong_bm25: f64,
    /// Cosine similarity above which a vector hit stands on its own.
    pub strong_cosine: f64,
}

impl Profile {
    /// Returns the settings this profile stands for.
    ///
    /// Lower thresholds admit weaker matches: useful when the user asked for
    /// precision and is willing to read more, harmful when they asked for speed.
    ///
    /// The cosine thresholds are measured, not guessed. On a 10,590-record
    /// database (see `examples/cosine_distribution.rs`) random pairs of records
    /// average 0.130 and pairs joined by a graph edge average 0.295 — nothing
    /// like the 0.5–0.6 baseline that static embeddings are often assumed to
    /// have. The thresholds this file shipped with rejected 85% of genuinely
    /// related pairs, which left the vector signal contributing almost nothing.
    /// Each profile now sits at a measured point: `min_cosine` on the curve
    /// where useful pairs still outnumber noise, `strong_cosine` where noise has
    /// all but vanished and one signal can carry a result by itself.
    pub fn settings(self) -> QualitySettings {
        match self {
            Self::Fast => QualitySettings {
                candidates_per_signal: 50,
                graph_hops: 0,
                budget_tokens: 300,
                min_cosine: 0.45,
                min_bm25: 0.10,
                strong_bm25: 0.60,
                strong_cosine: 0.70,
            },
            Self::Balanced => QualitySettings {
                candidates_per_signal: 200,
                graph_hops: 1,
                budget_tokens: 600,
                min_cosine: 0.35,
                min_bm25: 0.05,
                strong_bm25: 0.45,
                strong_cosine: 0.60,
            },
            Self::Precise => QualitySettings {
                candidates_per_signal: 500,
                graph_hops: 2,
                budget_tokens: 1200,
                min_cosine: 0.28,
                min_bm25: 0.03,
                strong_bm25: 0.30,
                strong_cosine: 0.50,
            },
            Self::Max => QualitySettings {
                candidates_per_signal: 1000,
                graph_hops: 2,
                budget_tokens: 2400,
                min_cosine: 0.20,
                min_bm25: 0.01,
                strong_bm25: 0.15,
                strong_cosine: 0.42,
            },
        }
    }

    /// Rough relative cost against the default profile, for showing the user
    /// what a change buys before they commit to it.
    pub fn relative_cost(self) -> f64 {
        let base = Self::Balanced.settings();
        let mine = self.settings();
        (mine.candidates_per_signal as f64 / base.candidates_per_signal as f64).mul_add(
            0.6,
            mine.budget_tokens as f64 / base.budget_tokens as f64 * 0.4,
        )
    }

    /// Every profile, for listing in the settings UI.
    pub fn all() -> [Self; 4] {
        [Self::Fast, Self::Balanced, Self::Precise, Self::Max]
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Precise => "precise",
            Self::Max => "max",
        };
        formatter.write_str(name)
    }
}

impl FromStr for Profile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fast" => Ok(Self::Fast),
            "balanced" | "default" => Ok(Self::Balanced),
            "precise" => Ok(Self::Precise),
            "max" | "maximum" => Ok(Self::Max),
            other => Err(format!(
                "unknown profile {other:?}; expected fast, balanced, precise, or max"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_grow_monotonically() {
        let mut previous = Profile::Fast.settings();
        for profile in [Profile::Balanced, Profile::Precise, Profile::Max] {
            let current = profile.settings();
            assert!(
                current.candidates_per_signal > previous.candidates_per_signal,
                "{profile} must widen the candidate pool"
            );
            assert!(
                current.budget_tokens > previous.budget_tokens,
                "{profile} must raise the answer budget"
            );
            assert!(
                current.min_cosine < previous.min_cosine,
                "{profile} must admit weaker vector matches"
            );
            assert!(
                current.strong_bm25 < previous.strong_bm25,
                "{profile} must let weaker lexical hits stand alone"
            );
            assert!(
                current.strong_cosine < previous.strong_cosine,
                "{profile} must let weaker vector hits stand alone"
            );
            previous = current;
        }
    }

    #[test]
    fn balanced_matches_shipped_defaults() {
        let settings = Profile::Balanced.settings();
        assert_eq!(settings.candidates_per_signal, 200);
        assert_eq!(settings.budget_tokens, 600);
        assert!((settings.min_cosine - 0.35).abs() < f64::EPSILON);
    }

    /// Measured on the live database: random pairs of records average 0.130
    /// cosine, and pairs joined by a graph edge run 0.239 at the median and
    /// 0.419 at the upper quartile. The spread is wide because edges are not all
    /// semantic — a session pointing at a record it touched carries no meaning
    /// to compare — so the median edge is the wrong thing to chase. What matters
    /// is that no profile sits down in the noise, and that the profiles the user
    /// reaches for still admit the edges that genuinely are about one topic.
    #[test]
    fn cosine_cutoffs_sit_between_noise_and_signal() {
        const MEASURED_RANDOM_MEAN: f64 = 0.130;
        const MEASURED_RELATED_UPPER_QUARTILE: f64 = 0.419;
        for profile in Profile::all() {
            let settings = profile.settings();
            assert!(
                settings.min_cosine > MEASURED_RANDOM_MEAN,
                "{profile} admits pairs no better than random"
            );
            if profile != Profile::Fast {
                assert!(
                    settings.min_cosine < MEASURED_RELATED_UPPER_QUARTILE,
                    "{profile} rejects even clearly related pairs"
                );
            }
            assert!(
                settings.strong_cosine > settings.min_cosine,
                "{profile} must demand more of a lone vector than of a confirmed one"
            );
        }
    }

    #[test]
    fn names_round_trip() {
        for profile in Profile::all() {
            let parsed: Profile = profile.to_string().parse().expect("name parses back");
            assert_eq!(parsed, profile);
        }
    }

    #[test]
    fn unknown_name_is_rejected_with_guidance() {
        let error = "turbo".parse::<Profile>().expect_err("must reject");
        assert!(error.contains("fast"), "error should list valid values");
    }

    #[test]
    fn cost_rises_with_quality() {
        assert!(Profile::Fast.relative_cost() < Profile::Balanced.relative_cost());
        assert!(Profile::Precise.relative_cost() > Profile::Balanced.relative_cost());
        assert!(Profile::Max.relative_cost() > Profile::Precise.relative_cost());
    }
}
