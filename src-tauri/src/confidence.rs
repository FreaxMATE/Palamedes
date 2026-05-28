//! Structural confidence — pure, testable scoring with no I/O.
//!
//! Palamedes deliberately does NOT ask the language model to rate its own
//! confidence in a belief. The research is unambiguous that self-reported LLM
//! confidence is badly miscalibrated and systematically overconfident
//! (verbalized-confidence ECE > 0.38; clustered at 0.9–1.0 regardless of
//! accuracy). Showing such a number would be false precision dressed as an
//! audit signal — exactly the failure mode this product exists to fight.
//!
//! Instead we derive confidence from *structural* signals the system observes
//! over time, which are the cross-session analogue of self-consistency /
//! semantic-entropy methods (the best training-free uncertainty estimators):
//!
//!   - trust class      — was the belief directly asserted by the user (with a
//!                         verified verbatim quote), inferred from explicit
//!                         context, or hypothesized from a weak/implicit signal?
//!   - reinforcement     — how many independent conversations re-stated it?
//!                         (a belief seen across 5 sessions is the cross-session
//!                          version of 5 agreeing samples)
//!   - recency           — has it gone stale without ever being reinforced?
//!
//! The continuous `score` drives sort order, map opacity, and the MCP
//! `min_confidence` filter. The `bucket` is what the UI shows — coarse on
//! purpose, because a precise decimal implies a calibration we don't have.
//!
//! IMPORTANT framing: this measures how *consistently the system holds* a
//! belief, NOT the probability it is *true*. A user who repeats a false
//! self-description, or an AI that repeatedly mis-infers the same thing, will
//! produce a high-consistency belief. Label it accordingly in the UI.

use serde::{Deserialize, Serialize};

/// Coarse confidence bucket. Stored continuously, shown bucketed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Strong,
    Moderate,
    Tentative,
}

impl Bucket {
    pub fn as_str(&self) -> &'static str {
        match self {
            Bucket::Strong => "strong",
            Bucket::Moderate => "moderate",
            Bucket::Tentative => "tentative",
        }
    }
}

/// What read paths attach to a belief DTO in place of a raw number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfidence {
    pub score: f64,
    pub bucket: Bucket,
}

// Tunable knobs. Kept as consts for now; the existing Settings panel already
// surfaces analogous thresholds (cosine/dedup) and these can move there later.
const BASE_ASSERTED: f64 = 0.70;
const BASE_INFERRED: f64 = 0.35;
const BASE_HYPOTHESIZED: f64 = 0.20;
/// Fallback base when trust class is unknown/garbage.
const BASE_UNKNOWN: f64 = 0.30;
/// Each independent reinforcement adds this much, up to `REINFORCE_CAP` edges.
const REINFORCE_STEP: f64 = 0.10;
const REINFORCE_CAP: i64 = 4;
/// A belief never reinforced and older than this (days) cools one tier.
const STALE_AFTER_DAYS: f64 = 120.0;
const STALE_PENALTY: f64 = 0.15;

const STRONG_AT: f64 = 0.70;
const MODERATE_AT: f64 = 0.40;

fn base_for(trust_class: &str) -> f64 {
    match trust_class {
        "asserted" => BASE_ASSERTED,
        "inferred" => BASE_INFERRED,
        "hypothesized" => BASE_HYPOTHESIZED,
        _ => BASE_UNKNOWN,
    }
}

/// Map a continuous score to a coarse bucket.
pub fn bucket(score: f64) -> Bucket {
    if score >= STRONG_AT {
        Bucket::Strong
    } else if score >= MODERATE_AT {
        Bucket::Moderate
    } else {
        Bucket::Tentative
    }
}

/// Structural score for a leaf belief.
///
/// `age_days` is the age of the most recent activity (last reinforcement, or
/// creation if never reinforced). `None` means unknown → no decay applied.
pub fn structural_score(trust_class: &str, reinforced_count: i64, age_days: Option<f64>) -> f64 {
    let base = base_for(trust_class);
    let reinforce = (reinforced_count.clamp(0, REINFORCE_CAP) as f64) * REINFORCE_STEP;
    let decay = match age_days {
        Some(days) if reinforced_count == 0 && days > STALE_AFTER_DAYS => STALE_PENALTY,
        _ => 0.0,
    };
    (base + reinforce - decay).clamp(0.0, 1.0)
}

/// Effective confidence for any belief.
///
/// Leaf beliefs are scored structurally. Summaries (trust_class == "summary")
/// carry a `stored` aggregate computed in Rust from their children's structural
/// scores at summarize time — never an LLM rating — so we use that directly.
pub fn effective(
    trust_class: &str,
    reinforced_count: i64,
    age_days: Option<f64>,
    stored: Option<f64>,
) -> EffectiveConfidence {
    let score = if trust_class == "summary" {
        stored.unwrap_or(0.5).clamp(0.0, 1.0)
    } else {
        structural_score(trust_class, reinforced_count, age_days)
    };
    EffectiveConfidence {
        score,
        bucket: bucket(score),
    }
}

/// Convenience wrapper used by all read paths: parses the activity timestamp
/// (last reinforcement, falling back to creation) and applies recency decay.
pub fn effective_for(
    trust_class: &str,
    reinforced_count: i64,
    created_at: &str,
    last_reinforced_at: Option<&str>,
    stored: Option<f64>,
) -> EffectiveConfidence {
    let activity = last_reinforced_at.unwrap_or(created_at);
    effective(trust_class, reinforced_count, age_days_since(activity), stored)
}

fn age_days_since(iso: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(iso).ok().map(|t| {
        (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_inferred_is_tentative() {
        let e = effective("inferred", 0, Some(0.0), None);
        assert_eq!(e.bucket, Bucket::Tentative);
        assert!((e.score - 0.35).abs() < 1e-9);
    }

    #[test]
    fn fresh_hypothesized_is_tentative() {
        assert_eq!(effective("hypothesized", 0, Some(0.0), None).bucket, Bucket::Tentative);
    }

    #[test]
    fn directly_asserted_starts_strong() {
        assert_eq!(effective("asserted", 0, Some(0.0), None).bucket, Bucket::Strong);
    }

    #[test]
    fn reinforcement_climbs_the_tiers() {
        // inferred: 0.35 base, +0.10 per reinforcement, cap at 4 edges.
        assert!((structural_score("inferred", 1, Some(0.0)) - 0.45).abs() < 1e-9);
        assert_eq!(bucket(structural_score("inferred", 2, Some(0.0))), Bucket::Moderate); // 0.55
        assert_eq!(bucket(structural_score("inferred", 4, Some(0.0))), Bucket::Strong); // 0.75
        // cap holds beyond 4 reinforcements.
        assert!((structural_score("inferred", 9, Some(0.0)) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn stale_unreinforced_belief_cools_one_tier() {
        // asserted 0.70 fresh -> Strong; stale & never reinforced -> 0.55 Moderate.
        let stale = structural_score("asserted", 0, Some(365.0));
        assert!((stale - 0.55).abs() < 1e-9);
        assert_eq!(bucket(stale), Bucket::Moderate);
    }

    #[test]
    fn reinforced_belief_does_not_decay() {
        // Reinforced beliefs are never penalized for age.
        let s = structural_score("inferred", 2, Some(365.0));
        assert!((s - 0.55).abs() < 1e-9);
    }

    #[test]
    fn summary_uses_stored_aggregate() {
        let e = effective("summary", 0, None, Some(0.82));
        assert!((e.score - 0.82).abs() < 1e-9);
        assert_eq!(e.bucket, Bucket::Strong);
    }

    #[test]
    fn unknown_trust_class_does_not_panic() {
        let e = effective("garbage", 0, Some(0.0), None);
        assert_eq!(e.bucket, Bucket::Tentative);
    }

    #[test]
    fn score_is_clamped() {
        assert!(structural_score("asserted", 100, Some(0.0)) <= 1.0);
        assert!(structural_score("hypothesized", 0, Some(99999.0)) >= 0.0);
    }
}
