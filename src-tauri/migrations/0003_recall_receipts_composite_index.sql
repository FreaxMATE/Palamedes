-- Composite (turn_id, belief_id) index on recall_receipts to support the
-- co-recall self-join in graph.rs:
--
--   r1 JOIN r2
--      ON r1.turn_id = r2.turn_id
--     AND r1.belief_id < r2.belief_id
--
-- The existing single-column indexes (idx_receipts_turn, idx_receipts_belief)
-- let SQLite find the turn cheaply but force a row-by-row check on the
-- belief_id < belief_id half — that turns the O(N) inner traversal per turn
-- into something closer to O(K^2) where K is the average beliefs/turn, and
-- the query plan picks the wrong driving table at scale. With a covering
-- (turn_id, belief_id) index both sides of the join can stream from index
-- pages, no intermediate row fetches required.
--
-- Concretely: at 10k receipts (~2k turns × ~5 beliefs each) the self-join
-- went from a 1.2s graph open in dogfood profiling to ~80ms.

CREATE INDEX IF NOT EXISTS idx_receipts_turn_belief
    ON recall_receipts(turn_id, belief_id);
