//! Structural confidence — Beta(α, β) posterior over how consistently
//! Palamedes holds a belief, with no I/O and no model self-report.
//!
//! ## Why Beta, not a linear formula
//!
//! The earlier version of this module summed a per-trust-class base, a
//! capped reinforcement step, and a flat stale penalty. That works but
//! it can't represent two things that matter for an audit-grade product:
//!
//! 1. **Asymmetric evidence weight.** A user *correcting* a belief is
//!    much stronger evidence than the AI restating it — the linear sum
//!    treated them as opposite-sign contributions of the same
//!    magnitude. Beta lets contradiction events shift the posterior far
//!    faster than support events.
//! 2. **A principled decay toward the prior.** The old formula's stale
//!    penalty was a single 0.15 step; the Beta version shrinks the
//!    accumulated evidence smoothly toward the trust-class prior, so
//!    "asserted six months ago, never reinforced" gradually returns to
//!    its starting trust rather than falling off a cliff.
//!
//! The data model itself is unchanged: confidence remains derived at
//! read time from observable structural signals, never stored or
//! self-reported. Inputs are now grouped in a [`Signals`] struct so
//! call sites read clearly when new signals come online.
//!
//! ## What this still does NOT measure
//!
//! It measures *how consistently the system holds a belief*, not the
//! probability the belief is true. A user who repeats a false self-
//! description, or an AI that repeatedly mis-infers the same thing,
//! will produce a high-consistency belief. The UI labels confidence
//! coarsely (strong / moderate / tentative) for this reason.

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
/// `score` is the Beta posterior mean α/(α+β); `bucket` is what the UI
/// renders.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EffectiveConfidence {
    pub score: f64,
    pub bucket: Bucket,
}

/// All observable signals that feed the posterior. Grouped in one
/// struct so adding a new signal doesn't require touching every call
/// site twice; missing signals default to zero / no observation.
#[derive(Debug, Clone, Default)]
pub struct Signals<'a> {
    pub trust_class: &'a str,
    /// `reinforced_by` provenance edges. Each is +α support.
    pub reinforced_count: i64,
    /// `contradicted_by` provenance edges. Each is +β refutation, 3×
    /// the weight of a support event by design.
    pub contradicted_count: i64,
    /// Canonical content-hash re-extractions of the same belief — the
    /// `beliefs.num_times` counter added in migration 0002. Each is a
    /// weak +α support (less than a full provenance edge, more than 0).
    pub num_times: i64,
    /// True when the belief has been corrected by the user — one-shot
    /// β contribution on top of any contradiction edges.
    pub is_corrected: bool,
    /// Age, in days, of the most recent activity (last reinforcement,
    /// or creation if never reinforced). Used for the decay term.
    pub age_days: Option<f64>,
    /// Aggregate score for summary beliefs, computed in Rust from the
    /// children's structural scores at summarize time. Bypasses the
    /// Beta math when `trust_class == "summary"`.
    pub stored: Option<f64>,
}

// -- Tunable constants -----------------------------------------------------
//
// These are the knobs that determine the posterior's shape. They are
// const for now; if a future Settings panel surfaces them, change the
// const to a static AtomicU64 and read it.

// Prior pseudocounts shaped by how the belief came in. The numbers are
// the (α, β) of a Beta distribution; trust = α/(α+β).
//   asserted     → (3, 1)  ≈ 0.75   user said it verbatim
//   inferred     → (1, 2)  ≈ 0.33   derived from context, hedge by default
//   hypothesized → (1, 3)  ≈ 0.25   weak guess from mood/tone
//   unknown      → (1, 1)  ≈ 0.50   neutral
const PRIOR_ASSERTED: (f64, f64) = (3.0, 1.0);
const PRIOR_INFERRED: (f64, f64) = (1.0, 2.0);
const PRIOR_HYPOTHESIZED: (f64, f64) = (1.0, 3.0);
const PRIOR_UNKNOWN: (f64, f64) = (1.0, 1.0);

/// Weight of one independent support observation (reinforcement edge
/// or canonical re-extraction).
const SUPPORT_WEIGHT: f64 = 0.5;

/// Weight of one contradiction observation. Asymmetric: a contradiction
/// hurts trust 3× more than a support event helps it. This is the
/// principled answer to "the AI keeps reinforcing something the user
/// already corrected once."
const CONTRADICTION_WEIGHT: f64 = 1.5;

/// One-time β contribution applied when `status == 'corrected'`.
const CORRECTED_WEIGHT: f64 = 1.0;

/// Age, in days, before idle decay starts pulling evidence back toward
/// the prior. Reinforced or recently-observed beliefs never enter the
/// decay regime.
const STALE_AFTER_DAYS: f64 = 120.0;

/// Per-day decay rate past `STALE_AFTER_DAYS`. At 0.005, an additional
/// 200 days of pure idle erases all accumulated evidence and the
/// belief returns exactly to its trust-class prior.
const DECAY_PER_DAY: f64 = 0.005;

// Bucket boundaries.
const STRONG_AT: f64 = 0.70;
const MODERATE_AT: f64 = 0.40;

// -------------------------------------------------------------------------

fn prior_for(trust_class: &str) -> (f64, f64) {
    match trust_class {
        "asserted" => PRIOR_ASSERTED,
        "inferred" => PRIOR_INFERRED,
        "hypothesized" => PRIOR_HYPOTHESIZED,
        _ => PRIOR_UNKNOWN,
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

/// Compute the Beta posterior (α, β) for a leaf belief from its signals.
/// Exposed mostly for tests + debug surfaces; production paths call
/// [`effective`].
pub fn beta_state(s: &Signals) -> (f64, f64) {
    let (mut alpha, mut beta) = prior_for(s.trust_class);

    // Support: reinforcement edges + content-hash re-extractions, both
    // valued at SUPPORT_WEIGHT each. Capped via the non-decay path —
    // very high counts are absorbed by the decay below if the belief
    // also goes idle, which is the realistic case.
    let support_events = s.reinforced_count.max(0) + s.num_times.saturating_sub(1).max(0);
    alpha += SUPPORT_WEIGHT * support_events as f64;

    // Refutation: contradiction edges weight CONTRADICTION_WEIGHT each
    // (3× a support event); a user correction adds CORRECTED_WEIGHT
    // once on top.
    beta += CONTRADICTION_WEIGHT * s.contradicted_count.max(0) as f64;
    if s.is_corrected {
        beta += CORRECTED_WEIGHT;
    }

    // Idle decay: shrink the accumulated evidence smoothly toward the
    // prior. Reinforced beliefs participate too — the old "reinforced
    // beliefs are immune to age forever" invariant was a sharp edge
    // that the Beta version replaces with monotone decay.
    if let Some(days) = s.age_days {
        let excess = (days - STALE_AFTER_DAYS).max(0.0);
        let shrink = (1.0 - DECAY_PER_DAY * excess).max(0.0);
        let (prior_a, prior_b) = prior_for(s.trust_class);
        alpha = prior_a + (alpha - prior_a) * shrink;
        beta = prior_b + (beta - prior_b) * shrink;
    }

    (alpha, beta)
}

/// Effective confidence for any belief.
///
/// Summaries (trust_class == "summary") carry a `stored` aggregate
/// computed in Rust from their children's structural scores at
/// summarize time, never an LLM rating, so the Beta math is bypassed.
pub fn effective(s: &Signals) -> EffectiveConfidence {
    let score = if s.trust_class == "summary" {
        s.stored.unwrap_or(0.5).clamp(0.0, 1.0)
    } else {
        let (alpha, beta) = beta_state(s);
        (alpha / (alpha + beta)).clamp(0.0, 1.0)
    };
    EffectiveConfidence {
        score,
        bucket: bucket(score),
    }
}

/// Convenience wrapper used by all read paths: builds a [`Signals`]
/// from row primitives and applies recency decay using the most-recent
/// activity timestamp.
pub fn effective_for(
    trust_class: &str,
    reinforced_count: i64,
    contradicted_count: i64,
    num_times: i64,
    is_corrected: bool,
    created_at: &str,
    last_reinforced_at: Option<&str>,
    last_observed_at: Option<&str>,
    stored: Option<f64>,
) -> EffectiveConfidence {
    let activity = last_observed_at
        .or(last_reinforced_at)
        .unwrap_or(created_at);
    effective(&Signals {
        trust_class,
        reinforced_count,
        contradicted_count,
        num_times,
        is_corrected,
        age_days: age_days_since(activity),
        stored,
    })
}

fn age_days_since(iso: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(iso).ok().map(|t| {
        (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(trust_class: &'static str) -> Signals<'static> {
        Signals {
            trust_class,
            age_days: Some(0.0),
            ..Default::default()
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fresh_priors_match_design() {
        assert!(approx(effective(&sig("asserted")).score, 0.75));
        assert!(approx(effective(&sig("inferred")).score, 1.0 / 3.0));
        assert!(approx(effective(&sig("hypothesized")).score, 0.25));
        assert!(approx(effective(&sig("unknown")).score, 0.5));
    }

    #[test]
    fn directly_asserted_starts_strong() {
        assert_eq!(effective(&sig("asserted")).bucket, Bucket::Strong);
    }

    #[test]
    fn fresh_inferred_and_hypothesized_are_tentative() {
        assert_eq!(effective(&sig("inferred")).bucket, Bucket::Tentative);
        assert_eq!(effective(&sig("hypothesized")).bucket, Bucket::Tentative);
    }

    #[test]
    fn reinforcement_climbs_monotonically() {
        let mut prev = 0.0;
        for n in 0..6 {
            let s = Signals {
                reinforced_count: n,
                ..sig("inferred")
            };
            let score = effective(&s).score;
            assert!(score >= prev, "expected monotone, got {score} after {prev}");
            prev = score;
        }
    }

    #[test]
    fn contradiction_outweighs_support_three_to_one() {
        // The 3× asymmetry is on the raw α/β increments — each support
        // event adds 0.5α (SUPPORT_WEIGHT); each contradiction adds 1.5β
        // (CONTRADICTION_WEIGHT = 3 × SUPPORT_WEIGHT). Score deltas
        // saturate non-linearly because trust is a ratio, but the raw
        // weight constants and the single-event |Δ| both behave as
        // designed.
        assert!((CONTRADICTION_WEIGHT - SUPPORT_WEIGHT * 3.0).abs() < 1e-9);

        let prior_score = effective(&sig("inferred")).score;
        let one_support = Signals {
            reinforced_count: 1,
            ..sig("inferred")
        };
        let one_contra = Signals {
            contradicted_count: 1,
            ..sig("inferred")
        };
        let delta_up = effective(&one_support).score - prior_score;
        let delta_down = prior_score - effective(&one_contra).score;
        assert!(delta_up > 0.0 && delta_down > 0.0);
        // Single contradiction moves the score farther than a single support.
        assert!(
            delta_down > delta_up,
            "expected |Δcontra| > |Δsupport|, got {delta_down} vs {delta_up}"
        );
    }

    #[test]
    fn user_correction_pushes_belief_down() {
        let plain = sig("inferred");
        let corrected = Signals {
            is_corrected: true,
            ..sig("inferred")
        };
        assert!(effective(&corrected).score < effective(&plain).score);
    }

    #[test]
    fn idle_belief_decays_toward_its_prior() {
        // Inferred + 4 reinforcements, freshly idle: trust = 1/(1+2*0.5)/... let's just
        // assert the directional property — at zero days reinforced > prior, and
        // at +5000 days idle it returns to the prior.
        let strong_fresh = Signals {
            reinforced_count: 4,
            age_days: Some(0.0),
            ..sig("inferred")
        };
        let strong_ancient = Signals {
            reinforced_count: 4,
            age_days: Some(5000.0),
            ..sig("inferred")
        };
        let prior_score = 1.0 / 3.0; // inferred prior
        assert!(effective(&strong_fresh).score > prior_score);
        assert!((effective(&strong_ancient).score - prior_score).abs() < 1e-9);
    }

    #[test]
    fn stale_decay_starts_after_threshold_only() {
        // 60 days of idle (under the 120 threshold) should leave evidence intact.
        let fresh = Signals {
            reinforced_count: 2,
            age_days: Some(0.0),
            ..sig("inferred")
        };
        let recent = Signals {
            reinforced_count: 2,
            age_days: Some(60.0),
            ..sig("inferred")
        };
        assert!((effective(&fresh).score - effective(&recent).score).abs() < 1e-9);
    }

    #[test]
    fn summary_uses_stored_aggregate() {
        let e = effective(&Signals {
            trust_class: "summary",
            stored: Some(0.82),
            ..Default::default()
        });
        assert!(approx(e.score, 0.82));
        assert_eq!(e.bucket, Bucket::Strong);
    }

    #[test]
    fn unknown_trust_class_does_not_panic() {
        let e = effective(&sig("garbage"));
        assert_eq!(e.score, 0.5);
        assert_eq!(e.bucket, Bucket::Moderate);
    }

    #[test]
    fn score_is_clamped() {
        let high = effective(&Signals {
            reinforced_count: 9999,
            ..sig("asserted")
        });
        assert!(high.score <= 1.0);
        let low = effective(&Signals {
            contradicted_count: 9999,
            ..sig("hypothesized")
        });
        assert!(low.score >= 0.0);
    }

    #[test]
    fn num_times_acts_as_weak_support() {
        // First observation (num_times = 1) doesn't change anything — it
        // is the initial creation event already captured by the prior.
        // Subsequent re-extractions add support.
        let one = Signals {
            num_times: 1,
            ..sig("inferred")
        };
        let many = Signals {
            num_times: 5,
            ..sig("inferred")
        };
        assert!((effective(&one).score - 1.0 / 3.0).abs() < 1e-9);
        assert!(effective(&many).score > effective(&one).score);
    }
}
