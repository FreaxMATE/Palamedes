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

    -- Short keyword label (1-4 words) for the memory map. Generated async
    -- after the belief lands. NULL until the labeler runs; the UI falls
    -- back to a truncated statement when missing.
    label              TEXT,

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
    -- Confidence is NOT self-reported by the model. It is derived structurally
    -- at read time (see src/confidence.rs) from trust class + reinforcement +
    -- recency, so leaf versions store NULL here. Summaries store a Rust-computed
    -- aggregate of their children's structural scores. Nullable on purpose.
    confidence  REAL    CHECK (confidence IS NULL OR (confidence >= 0.0 AND confidence <= 1.0)),
    reason      TEXT,                                     -- why this version exists
    editor      TEXT    NOT NULL CHECK (editor IN ('user','ai','system')),
    created_at  TEXT    NOT NULL,
    UNIQUE (belief_id, version_num)
);

CREATE INDEX IF NOT EXISTS idx_belief_versions_belief ON belief_versions(belief_id);

-- ============================================================================
-- Provenance edges: each belief version points at the sources that produced
-- or affected it. For summaries, sources are child beliefs (relation='summarizes').
-- source_type drives the foreign lookup:
--   'turn'       -> messages
--   'artifact'   -> artifacts
--   'belief'     -> beliefs
--   'proposal'   -> belief_proposals (a user-accepted external proposal)
--   'mcp_client' -> mcp_clients      (external AI that touched this version)
-- FK is not declared because the target table varies.
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_provenance (
    id                TEXT PRIMARY KEY,
    belief_version_id TEXT NOT NULL REFERENCES belief_versions(id) ON DELETE CASCADE,
    source_type       TEXT NOT NULL CHECK (source_type IN
                        ('turn','artifact','belief','proposal','mcp_client')),
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
-- Merge log: one row per belief merge (auto or manual) so the audit panel can
-- show a "recently merged" digest and offer a one-click Undo. Each row captures
-- enough to fully reverse the merge: the absorbed belief's prior status +
-- version, the tombstone version the merge wrote, the provenance edges copied
-- into the keeper, and the absorbed embedding (so retrieval can be restored).
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_merges (
    id                        TEXT PRIMARY KEY,
    keeper_id                 TEXT NOT NULL,
    keeper_statement          TEXT NOT NULL,
    absorbed_id               TEXT NOT NULL,
    absorbed_statement        TEXT NOT NULL,
    absorbed_prior_status     TEXT NOT NULL,
    absorbed_prior_version_id TEXT NOT NULL,
    tombstone_version_id      TEXT NOT NULL,
    copied_prov_ids           TEXT NOT NULL DEFAULT '[]', -- JSON array of belief_provenance.id copied into keeper
    absorbed_embedding        BLOB,                        -- absorbed's vec row, to restore on undo (NULL if none)
    cosine                    REAL,
    kind                      TEXT NOT NULL CHECK (kind IN ('auto','manual')),
    created_at                TEXT NOT NULL,
    reverted_at               TEXT
);

CREATE INDEX IF NOT EXISTS idx_belief_merges_created ON belief_merges(created_at);

-- ============================================================================
-- Merge dismissals: pairs the user reviewed and declared NOT duplicates
-- ("keep separated"). Excluded from every future merge-candidate sweep so the
-- same pair stops resurfacing — and so the auto-merge sweep never collapses
-- them either. Stored ordered (belief_a_id < belief_b_id) so the lookup key is
-- stable regardless of which side was A or B in a given scan. CASCADE so a
-- dismissal vanishes if either belief is later deleted.
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_merge_dismissals (
    belief_a_id TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    belief_b_id TEXT NOT NULL REFERENCES beliefs(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (belief_a_id, belief_b_id)
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
-- MCP server (Phase C).
-- An external AI tool (Claude Desktop, Cursor, Witsy, ...) connects via the
-- Model Context Protocol and is recorded here. Consent is per-client and
-- split into two buckets: read (list/get/search) and write (propose/correct).
-- Each is NULL until the user decides, then 0 (denied) or 1 (granted).
-- ============================================================================

CREATE TABLE IF NOT EXISTS mcp_clients (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL UNIQUE,    -- from MCP initialize: clientInfo.name
    version       TEXT,                    -- clientInfo.version
    client_info   TEXT,                    -- full JSON for forensics
    consent_read  INTEGER,                 -- NULL=pending, 0=denied, 1=granted
    consent_write INTEGER,
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT,
    revoked_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_mcp_clients_name ON mcp_clients(name);

-- External proposals live OUTSIDE the ledger until accepted, so retrieval,
-- the map, and summarization never see unreviewed external claims. On accept,
-- a real belief is materialized via the existing Ledger path with provenance
-- (source_type='proposal', source_id=<belief_proposals.id>).
--
-- kind='propose' rows carry a new statement; kind='correct' rows point at
-- target_belief_id and suggest a status change with a reason.

CREATE TABLE IF NOT EXISTS belief_proposals (
    id                   TEXT PRIMARY KEY,
    client_id            TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    kind                 TEXT NOT NULL CHECK (kind IN ('propose','correct')),

    -- propose fields
    statement            TEXT,
    suggested_category   TEXT,
    suggested_confidence REAL CHECK (suggested_confidence IS NULL
                                     OR (suggested_confidence >= 0.0
                                         AND suggested_confidence <= 1.0)),
    reasoning            TEXT,             -- AI's "why I'm proposing this"
    source               TEXT,             -- AI's free-form source label

    -- correct fields
    target_belief_id     TEXT REFERENCES beliefs(id) ON DELETE SET NULL,
    suggested_status     TEXT CHECK (suggested_status IS NULL OR suggested_status IN
                                     ('contested','corrected','expired')),
    correction_reason    TEXT,

    -- lifecycle
    status               TEXT NOT NULL DEFAULT 'pending'
                         CHECK (status IN ('pending','accepted','rejected','superseded')),
    decided_at           TEXT,
    decided_belief_id    TEXT REFERENCES beliefs(id) ON DELETE SET NULL,
    created_at           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_proposals_status ON belief_proposals(status, created_at);
CREATE INDEX IF NOT EXISTS idx_proposals_client ON belief_proposals(client_id);
CREATE INDEX IF NOT EXISTS idx_proposals_target ON belief_proposals(target_belief_id);

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

-- ============================================================================
-- 2D projection cache for the memory map.
-- One row per belief whose embedding has been projected. Stale rows (where
-- projection_version < the table-wide current version) are tolerated; the
-- frontend re-projects when too many beliefs lack positions. The position
-- range is conventionally [-1, 1] after the renderer normalizes.
-- ============================================================================

CREATE TABLE IF NOT EXISTS belief_positions (
    belief_id          TEXT PRIMARY KEY REFERENCES beliefs(id) ON DELETE CASCADE,
    x                  REAL NOT NULL,
    y                  REAL NOT NULL,
    projection_version INTEGER NOT NULL,
    created_at         TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_belief_positions_version ON belief_positions(projection_version);

-- ============================================================================
-- MCP read audit log.
-- Every successful invocation of a read tool (list_beliefs, get_belief,
-- search_beliefs) appends one row, attributed to the connecting MCP client.
-- Two jobs:
--   1. Rate limit — a client granted consent_read could otherwise enumerate
--      the whole ledger via repeated calls. The audit module counts rows
--      from the last 24h per client and blocks new reads above the budget
--      (settings.mcp_read_limit_per_day; default 1000).
--   2. Forensic surface — the Audit panel (week 4) shows the user exactly
--      which beliefs each client has read, when, and via which tool.
-- See src/mcp/audit.rs for caps on query and returned_ids size.
-- ============================================================================

CREATE TABLE IF NOT EXISTS mcp_read_log (
    id            TEXT PRIMARY KEY,
    client_id     TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    tool          TEXT NOT NULL CHECK (tool IN ('list_beliefs','get_belief','search_beliefs')),
    query         TEXT,                       -- JSON of input args, capped server-side
    result_count  INTEGER NOT NULL DEFAULT 0,
    returned_ids  TEXT,                       -- JSON array of belief ids, capped server-side
    created_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_mcp_read_log_client_time
    ON mcp_read_log(client_id, created_at);
