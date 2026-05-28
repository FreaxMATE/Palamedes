-- Belief content-hash + observation counters (ANALYSIS.md §3.2.2,
-- Memori-inspired pattern).
--
-- `uniq` is SHA-256(canonicalize(subject)) — collapses chatty
-- re-extractions of the same statement into one row. Populated by
-- the Rust insert path going forward; a separate startup backfill
-- pass fills it in for rows that pre-date this migration.
--
-- `num_times` is the count of independent observations of this
-- belief. Bumped by the upsert path when an existing uniq matches.
-- Feeds the Beta(α,β) trust math landing in week 2 — `recall_hit`
-- weighting reads this directly.
--
-- `last_observed_at` is the most recent observation timestamp, used
-- by the recency-decay branch of confidence.rs alongside
-- last_reinforced_at.
--
-- The index is intentionally NON-unique. Auto-merge handles the
-- semantic dedupe path; this column is for cheap exact-text lookup
-- and observation-counter rollup, not enforcement.

ALTER TABLE beliefs ADD COLUMN uniq TEXT;
ALTER TABLE beliefs ADD COLUMN num_times INTEGER NOT NULL DEFAULT 1;
ALTER TABLE beliefs ADD COLUMN last_observed_at TEXT;

CREATE INDEX IF NOT EXISTS idx_beliefs_uniq
    ON beliefs(uniq)
    WHERE uniq IS NOT NULL;
