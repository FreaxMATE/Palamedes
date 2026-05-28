-- Persistent co-recall edges (ANALYSIS.md §4 week 2 / §2.3).
--
-- The memory map previously computed the "two beliefs cited together
-- in the same turn" graph by self-joining recall_receipts on every
-- map open. With the (turn_id, belief_id) composite index from
-- migration 0003 that became cheap up to a few-K-receipt corpus, but
-- it still scales as O(receipts^2/turn) and recomputes the same set
-- on every open.
--
-- This migration materializes the result into a tiny derived table
-- that the chat pipeline appends to on each new turn. The map then
-- reads `belief_co_recall` directly — O(rows) on map open, no joins.
--
-- The CHECK on canonical order (a < b) means there's exactly one row
-- per unordered pair, so the upsert path doesn't need to dedupe at
-- write time.

CREATE TABLE IF NOT EXISTS belief_co_recall (
    belief_a_id TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    belief_b_id TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    weight      INTEGER NOT NULL DEFAULT 1,
    last_seen   TEXT NOT NULL,
    PRIMARY KEY (belief_a_id, belief_b_id),
    CHECK (belief_a_id < belief_b_id)
);

CREATE INDEX IF NOT EXISTS idx_co_recall_b ON belief_co_recall(belief_b_id);

-- Backfill from the existing recall_receipts. CASE handles the order
-- swap, COUNT(*) and MAX(created_at) collapse repeated co-occurrences
-- across turns into a single (a, b) row.
INSERT OR IGNORE INTO belief_co_recall (belief_a_id, belief_b_id, weight, last_seen)
SELECT
    CASE WHEN r1.belief_id < r2.belief_id THEN r1.belief_id ELSE r2.belief_id END AS a,
    CASE WHEN r1.belief_id < r2.belief_id THEN r2.belief_id ELSE r1.belief_id END AS b,
    COUNT(*) AS weight,
    -- recall_receipts has no created_at column; use the turn's message
    -- created_at as a proxy. Safe fallback to '' for the rare receipt
    -- whose parent message vanished.
    COALESCE(MAX(m.created_at), '') AS last_seen
FROM recall_receipts r1
JOIN recall_receipts r2
  ON r1.turn_id = r2.turn_id
 AND r1.belief_id <> r2.belief_id
LEFT JOIN messages m ON m.id = r1.turn_id
GROUP BY a, b;
