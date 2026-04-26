-- Palamedes schema.
-- Idempotent: safe to run on every startup.
-- All IDs are TEXT UUIDs for consistency across tables.
--
-- Existing tables (conversations, messages, settings) hold the chat tree.
-- New tables (artifacts, beliefs, belief_versions, belief_provenance,
-- belief_blocklist, recall_receipts, extraction_log) hold the Belief Ledger.
-- Provenance into chat uses source_type='turn' + source_id=messages.id.

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;

-- ============================================================================
-- Chat tree (existing)
-- ============================================================================

CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    current_leaf_id TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    parent_id       TEXT REFERENCES messages(id),
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    branch_title    TEXT,
    created_at      TEXT NOT NULL,
    model           TEXT,
    tokens_in       INTEGER,
    tokens_out      INTEGER,
    cost_micro_usd  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_messages_conv   ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_parent ON messages(parent_id);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ============================================================================
-- Artifacts: notes, clips, files, voice transcripts (Phase 2+ populated)
-- ============================================================================

CREATE TABLE IF NOT EXISTS artifacts (
    id         TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('note','clip','file','voice')),
    title      TEXT,
    content    TEXT,          -- inline text; NULL for binaries
    path       TEXT,          -- on-disk path for binaries
    source_url TEXT,
    created_at TEXT NOT NULL
);

-- ============================================================================
-- Belief Ledger
-- A belief is a claim the system holds about some subject (default: 'user').
-- Summaries are beliefs at level >= 1 whose provenance points at child beliefs.
-- ============================================================================

CREATE TABLE IF NOT EXISTS beliefs (
    id                 TEXT PRIMARY KEY,
    subject            TEXT NOT NULL DEFAULT 'user',
    category           TEXT,          -- preference | fact | skill | plan | context | other
    current_version_id TEXT,          -- set after first version is written
    status             TEXT NOT NULL CHECK (status IN
                         ('asserted','inferred','corrected','contested','expired','blocked')),
    trust_class        TEXT NOT NULL CHECK (trust_class IN
                         ('asserted','inferred','hypothesized','summary')),
    scope              TEXT NOT NULL DEFAULT 'global' CHECK (scope IN
                         ('global','branch_local','branch_isolated')),
    scope_ref_id       TEXT,          -- message id (branch root) when scope != 'global'

    -- Hierarchy: 0 = leaf, 1+ = summary of lower levels.
    level              INTEGER NOT NULL DEFAULT 0,
    parent_summary_id  TEXT REFERENCES beliefs(id),

    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    last_reinforced_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_beliefs_subject  ON beliefs(subject);
CREATE INDEX IF NOT EXISTS idx_beliefs_status   ON beliefs(status);
CREATE INDEX IF NOT EXISTS idx_beliefs_level    ON beliefs(level);
CREATE INDEX IF NOT EXISTS idx_beliefs_parent   ON beliefs(parent_summary_id);
CREATE INDEX IF NOT EXISTS idx_beliefs_scope    ON beliefs(scope, scope_ref_id);

CREATE TABLE IF NOT EXISTS belief_versions (
    id          TEXT PRIMARY KEY,
    belief_id   TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    version_num INTEGER NOT NULL,
    statement   TEXT    NOT NULL,
    confidence  REAL    NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    reason      TEXT,                                     -- why this version exists
    editor      TEXT    NOT NULL CHECK (editor IN ('user','ai','system')),
    created_at  TEXT    NOT NULL,
    UNIQUE (belief_id, version_num)
);

CREATE INDEX IF NOT EXISTS idx_belief_versions_belief ON belief_versions(belief_id);

-- ============================================================================
-- Provenance edges: each belief version points at the sources that produced
-- or affected it. For summaries, sources are child beliefs (relation='summarizes').
-- source_type drives the foreign lookup: 'turn' -> messages, 'artifact' -> artifacts,
-- 'belief' -> beliefs. FK is not declared because the target varies.
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_provenance (
    id                TEXT PRIMARY KEY,
    belief_version_id TEXT NOT NULL REFERENCES belief_versions(id) ON DELETE CASCADE,
    source_type       TEXT NOT NULL CHECK (source_type IN ('turn','artifact','belief')),
    source_id         TEXT NOT NULL,
    relation          TEXT NOT NULL CHECK (relation IN
                        ('extracted_from','reinforced_by','contradicted_by',
                         'corrected_by','summarizes')),
    created_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_provenance_version ON belief_provenance(belief_version_id);
CREATE INDEX IF NOT EXISTS idx_provenance_source  ON belief_provenance(source_type, source_id);

-- ============================================================================
-- Blocklist: patterns or specific beliefs that extraction must not re-infer.
-- Populated when the user hard-pins a correction as wrong.
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_blocklist (
    id         TEXT PRIMARY KEY,
    pattern    TEXT,                                          -- natural-language pattern
    belief_id  TEXT REFERENCES beliefs(id) ON DELETE SET NULL,
    reason     TEXT,
    created_at TEXT NOT NULL,
    CHECK (pattern IS NOT NULL OR belief_id IS NOT NULL)
);

-- ============================================================================
-- Recall receipts: which beliefs were retrieved for each assistant turn.
-- Powers the "receipts" UI and retrospective audits.
-- ============================================================================

CREATE TABLE IF NOT EXISTS recall_receipts (
    id                TEXT PRIMARY KEY,
    turn_id           TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    belief_id         TEXT NOT NULL REFERENCES beliefs(id),
    belief_version_id TEXT NOT NULL REFERENCES belief_versions(id),
    weight            REAL NOT NULL,
    rank              INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_receipts_turn   ON recall_receipts(turn_id);
CREATE INDEX IF NOT EXISTS idx_receipts_belief ON recall_receipts(belief_id);

-- ============================================================================
-- Extraction log: every extraction attempt, for auditing and prompt tuning.
-- ============================================================================

CREATE TABLE IF NOT EXISTS extraction_log (
    id         TEXT PRIMARY KEY,
    turn_id    TEXT REFERENCES messages(id) ON DELETE CASCADE,
    status     TEXT NOT NULL CHECK (status IN ('ok','failed','empty')),
    model      TEXT,
    raw_output TEXT,
    error      TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_extraction_log_turn ON extraction_log(turn_id);

-- ============================================================================
-- Embeddings (Phase 5).
-- One row per belief id. Hardcoded dim must match `EMBEDDING_DIM` in
-- src/embeddings.rs and the embedding model configured in `settings.embedding_model`.
-- Switching to a different-sized model requires dropping this table and
-- re-embedding from scratch.
-- ============================================================================

CREATE VIRTUAL TABLE IF NOT EXISTS vec_beliefs USING vec0(
    belief_id TEXT PRIMARY KEY,
    embedding FLOAT[4096]
);
