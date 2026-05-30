import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Role = "user" | "assistant" | "system";

export interface Conversation {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  current_leaf_id: string | null;
}

export interface Message {
  id: string;
  conversation_id: string;
  parent_id: string | null;
  role: Role;
  content: string;
  branch_title: string | null;
  created_at: string;
  model: string | null;
  tokens_in: number | null;
  tokens_out: number | null;
  cost_micro_usd: number | null;
}

interface StreamChunk {
  stream_id: string;
  delta: string;
}
interface StreamReasoning {
  stream_id: string;
  delta: string;
}
interface StreamDone {
  stream_id: string;
  message_id: string;
}
interface StreamError {
  stream_id: string;
  error: string;
}

export interface StreamHandle {
  streamId: string;
  done: Promise<string>; // resolves with assistant message id
  cancel: () => Promise<void>;
}

// ---------- CRUD ----------

export const listConversations = () =>
  invoke<Conversation[]>("list_conversations");

export const createConversation = (title: string) =>
  invoke<Conversation>("create_conversation", { title });

export const deleteConversation = (id: string) =>
  invoke<void>("delete_conversation", { id });

export const wipeChats = () => invoke<void>("wipe_chats");

export const wipeAllData = () => invoke<void>("wipe_all_data");

export const renameConversation = (id: string, title: string) =>
  invoke<void>("rename_conversation", { id, title });

export const getMessages = (conversationId: string) =>
  invoke<Message[]>("get_messages", { conversationId });

export const setCurrentLeaf = (conversationId: string, leafId: string | null) =>
  invoke<void>("set_current_leaf", { conversationId, leafId });

export const setBranchTitle = (messageId: string, title: string) =>
  invoke<void>("set_branch_title", { messageId, title });

export const deepestDescendant = (messageId: string) =>
  invoke<string>("deepest_descendant", { messageId });

export const getSetting = (key: string) =>
  invoke<string | null>("get_setting", { key });

export const setSetting = (key: string, value: string) =>
  invoke<void>("set_setting", { key, value });

export const listModels = () => invoke<string[]>("list_models");

// ---------- LLM provider configuration ----------

export interface LlmProviderConfig {
  base_url: string;
  /** Whether an API key is currently saved. The actual key value is
   *  never returned over the IPC bridge — it stays in the settings DB. */
  api_key_set: boolean;
}

export const getLlmProvider = () =>
  invoke<LlmProviderConfig>("get_llm_provider");

/** Save the provider endpoint + API key. Pass `apiKey = null` to leave
 *  the existing key untouched (e.g. when only changing the base URL).
 *  The new client takes effect after the next app restart. */
export const setLlmProvider = (baseUrl: string, apiKey: string | null) =>
  invoke<void>("set_llm_provider", { baseUrl, apiKey });

/** Curated provider presets. Picking one fills in the base URL; the
 *  user pastes their API key separately. `model_hint` is informational
 *  — what name to expect in the model picker once `list_models` runs. */
export interface ProviderPreset {
  id: string;
  label: string;
  base_url: string;
  /** Where the user gets a key. */
  signup_url: string;
  /** Free-text note shown under the preset (e.g. "covers Claude + Gemini"). */
  note?: string;
}

export const PROVIDER_PRESETS: ProviderPreset[] = [
  {
    id: "openai",
    label: "OpenAI (ChatGPT)",
    base_url: "https://api.openai.com/v1",
    signup_url: "https://platform.openai.com/api-keys",
    note: "GPT-4o, GPT-5. Native OpenAI endpoint.",
  },
  {
    id: "anthropic",
    label: "Anthropic (Claude)",
    base_url: "https://api.anthropic.com/v1",
    signup_url: "https://console.anthropic.com/settings/keys",
    note: "Claude 4.x via Anthropic's OpenAI-compatible adapter.",
  },
  {
    id: "google",
    label: "Google (Gemini)",
    base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
    signup_url: "https://aistudio.google.com/apikey",
    note: "Gemini via Google's OpenAI-compatible endpoint.",
  },
  {
    id: "openrouter",
    label: "OpenRouter",
    base_url: "https://openrouter.ai/api/v1",
    signup_url: "https://openrouter.ai/keys",
    note: "One key, ~200 models — Claude, GPT, Gemini, Llama, Mistral.",
  },
  {
    id: "groq",
    label: "Groq",
    base_url: "https://api.groq.com/openai/v1",
    signup_url: "https://console.groq.com/keys",
    note: "Very fast Llama, Mixtral, Qwen.",
  },
  {
    id: "cerebras",
    label: "Cerebras",
    base_url: "https://api.cerebras.ai/v1",
    signup_url: "https://cloud.cerebras.ai/platform/",
    note: "Even faster Llama 3 inference.",
  },
  {
    id: "together",
    label: "Together AI",
    base_url: "https://api.together.xyz/v1",
    signup_url: "https://api.together.xyz/settings/api-keys",
    note: "Llama 3/4, Mixtral, Qwen, DeepSeek.",
  },
  {
    id: "fireworks",
    label: "Fireworks",
    base_url: "https://api.fireworks.ai/inference/v1",
    signup_url: "https://fireworks.ai/account/api-keys",
    note: "Llama 3, DeepSeek, Mixtral.",
  },
  {
    id: "nebius",
    label: "Nebius Token Factory",
    base_url: "https://api.tokenfactory.nebius.com/v1",
    signup_url: "https://studio.nebius.ai/",
    note: "Default. Kimi K2.5, Qwen3-Embedding-8B.",
  },
  {
    id: "ollama",
    label: "Ollama (local)",
    base_url: "http://localhost:11434/v1",
    signup_url: "https://ollama.com/download",
    note: "Run models locally. No API key needed — use any non-empty placeholder.",
  },
  {
    id: "lmstudio",
    label: "LM Studio (local)",
    base_url: "http://localhost:1234/v1",
    signup_url: "https://lmstudio.ai/",
    note: "Local GUI for running open models. Use any non-empty placeholder key.",
  },
];

// ---------- Belief Ledger / audit ----------

export type BeliefStatus =
  | "asserted"
  | "inferred"
  | "corrected"
  | "contested"
  | "expired"
  | "blocked";

export type TrustClass = "asserted" | "inferred" | "hypothesized" | "summary";

// Confidence is never a self-reported number. It is derived structurally
// (reinforcement + recency + trust class) and surfaced as a coarse bucket.
export type ConfidenceBucket = "strong" | "moderate" | "tentative";

export interface AuditBelief {
  id: string;
  statement: string;
  /** Structural score [0,1] — for sorting/opacity, not display. */
  effective_confidence: number;
  /** Coarse bucket shown in the UI. */
  confidence_bucket: ConfidenceBucket;
  category: string | null;
  status: BeliefStatus;
  trust_class: TrustClass;
  level: number;
  parent_summary_id: string | null;
  provenance_count: number;
  version_count: number;
  reinforced_count: number;
  created_at: string;
  updated_at: string;
}

export interface ProvenanceItem {
  source_type: "turn" | "artifact" | "belief" | "proposal" | "mcp_client";
  source_id: string;
  relation:
    | "extracted_from"
    | "reinforced_by"
    | "contradicted_by"
    | "corrected_by"
    | "summarizes";
  preview: string | null;
  /** For turn sources: the conversation to deep-link to. */
  conversation_id: string | null;
  created_at: string;
}

export interface VersionItem {
  version_num: number;
  statement: string;
  reason: string | null;
  editor: "user" | "ai" | "system";
  created_at: string;
  provenance: ProvenanceItem[];
}

export interface BeliefDetail {
  belief: AuditBelief;
  versions: VersionItem[];
}

export interface UpdateBeliefArgs {
  id: string;
  newStatus?: BeliefStatus;
  newTrustClass?: TrustClass;
  newStatement?: string;
  reason?: string;
  blocklistPattern?: string;
}

export const listBeliefsAudit = () => invoke<AuditBelief[]>("list_beliefs_audit");

export const getBeliefDetail = (id: string) =>
  invoke<BeliefDetail>("get_belief_detail", { id });

export const updateBelief = (args: UpdateBeliefArgs) =>
  invoke<void>("update_belief", { args });

export interface SummarizeReport {
  categories_processed: number;
  summaries_created: number;
  beliefs_covered: number;
  overlaps_dropped: number;
  hallucinations_dropped: number;
  errors: string[];
}

export const summarizeNow = () => invoke<SummarizeReport>("summarize_now");

export interface EmbedReport {
  embedded: number;
  failed: number;
  first_error: string | null;
  model: string;
}

export const embedUnembeddedBeliefs = () =>
  invoke<EmbedReport>("embed_unembedded_beliefs");

export interface LabelBackfillReport {
  labeled: number;
  failed: number;
  first_error: string | null;
  model: string;
}

export const regenerateBeliefLabels = () =>
  invoke<LabelBackfillReport>("regenerate_belief_labels");

export interface ReceiptItem {
  belief_id: string;
  statement: string;
  trust_class: TrustClass;
  status: BeliefStatus;
  weight: number;
  rank: number;
}

export const getReceiptsForTurn = (turnId: string) =>
  invoke<ReceiptItem[]>("get_receipts_for_turn", { turnId });

export interface RecapResponse {
  date: string;
  path: string;
  markdown: string;
}

export const generateRecap = (dateIso?: string) =>
  invoke<RecapResponse>("generate_recap", { dateIso: dateIso ?? null });

export const listRecaps = () => invoke<string[]>("list_recaps");

export const nameCluster = (statements: string[]) =>
  invoke<string>("name_cluster", { statements });

export interface MergeCandidate {
  a_id: string;
  a_statement: string;
  a_status: BeliefStatus;
  a_trust_class: TrustClass;
  b_id: string;
  b_statement: string;
  b_status: BeliefStatus;
  b_trust_class: TrustClass;
  cosine: number;
  tier: "definite" | "likely";
}

export const listMergeCandidates = () =>
  invoke<MergeCandidate[]>("list_merge_candidates");

export interface MergeBeliefsArgs {
  keeperId: string;
  absorbedId: string;
  reason?: string;
}

export const mergeBeliefs = (args: MergeBeliefsArgs) =>
  invoke<void>("merge_beliefs", { args });

/** Mark a candidate pair as "not a duplicate" — it stops surfacing in the
 *  review list and is never auto-merged. */
export const dismissMergeCandidate = (aId: string, bId: string) =>
  invoke<void>("dismiss_merge_candidate", { args: { aId, bId } });

/** Auto-merge the "definite" near-duplicates in the background. Returns count. */
export const autoMergeDuplicates = () =>
  invoke<number>("auto_merge_duplicates");

/** One-click batch: merge every currently-surfaced candidate pair. */
export const mergeAllCandidates = () =>
  invoke<number>("merge_all_candidates");

export interface MergeRecord {
  id: string;
  keeper_id: string;
  keeper_statement: string;
  absorbed_id: string;
  absorbed_statement: string;
  cosine: number | null;
  kind: "auto" | "manual";
  created_at: string;
}

/** Recent, not-yet-reverted merges — the digest with Undo. */
export const recentMerges = (limit?: number) =>
  invoke<MergeRecord[]>("recent_merges", { limit: limit ?? null });

/** Reverse a merge: restores the absorbed belief and re-indexes it. */
export const undoMerge = (mergeId: string) =>
  invoke<void>("undo_merge", { mergeId });

// ---------- memory map ----------

export interface GraphBelief {
  id: string;
  statement: string;
  label: string | null;
  category: string | null;
  status: BeliefStatus;
  trust_class: TrustClass;
  level: number;
  parent_summary_id: string | null;
  confidence: number;
  confidence_bucket: ConfidenceBucket;
  reinforced_count: number;
  created_at: string;
  last_reinforced_at: string | null;
  x: number | null;
  y: number | null;
  has_embedding: boolean;
}

export type GraphEdgeKind =
  | "hierarchy"
  | "summarizes"
  | "reinforced_by"
  | "contradicted_by"
  | "corrected_by"
  | "extracted_from"
  | "knn"
  | "co_recall";

export interface GraphEdge {
  source_id: string;
  target_id: string;
  kind: GraphEdgeKind;
  weight: number;
}

export interface GraphSnapshot {
  beliefs: GraphBelief[];
  edges: GraphEdge[];
  projection_version: number;
}

export const getGraphSnapshot = () => invoke<GraphSnapshot>("get_graph_snapshot");

export const getGraphEdgesExtended = (knnK?: number) =>
  invoke<GraphEdge[]>("get_graph_edges_extended", { knnK: knnK ?? 3 });

export interface TurnForBelief {
  turn_id: string;
  conversation_id: string;
  conversation_title: string;
  preview: string;
  created_at: string;
  weight: number;
  rank: number;
}

export const getTurnsForBelief = (beliefId: string) =>
  invoke<TurnForBelief[]>("get_turns_for_belief", { beliefId });

export const getBeliefEmbeddings = (beliefIds: string[]) =>
  invoke<Array<[string, number[]]>>("get_belief_embeddings", { beliefIds });

export interface PositionUpdate {
  belief_id: string;
  x: number;
  y: number;
}

export const saveBeliefPositions = (positions: PositionUpdate[]) =>
  invoke<number>("save_belief_positions", { positions });

// ---------- streaming ----------

type InvokeCall = [command: string, args: Record<string, unknown>];

function startStream(
  [command, args]: InvokeCall,
  onDelta: (text: string) => void,
  onReasoning?: (text: string) => void,
): StreamHandle {
  const streamId = crypto.randomUUID();

  let resolveDone!: (messageId: string) => void;
  let rejectDone!: (err: unknown) => void;
  const donePromise = new Promise<string>((res, rej) => {
    resolveDone = res;
    rejectDone = rej;
  });

  const unlisteners: UnlistenFn[] = [];
  function cleanup() {
    for (const u of unlisteners) u();
    unlisteners.length = 0;
  }

  (async () => {
    unlisteners.push(
      await listen<StreamChunk>("stream_chunk", (e) => {
        if (e.payload.stream_id === streamId) onDelta(e.payload.delta);
      }),
    );
    if (onReasoning) {
      unlisteners.push(
        await listen<StreamReasoning>("stream_reasoning", (e) => {
          if (e.payload.stream_id === streamId) onReasoning(e.payload.delta);
        }),
      );
    }
    unlisteners.push(
      await listen<StreamDone>("stream_done", (e) => {
        if (e.payload.stream_id === streamId) {
          cleanup();
          resolveDone(e.payload.message_id);
        }
      }),
    );
    unlisteners.push(
      await listen<StreamError>("stream_error", (e) => {
        if (e.payload.stream_id === streamId) {
          cleanup();
          rejectDone(new Error(e.payload.error));
        }
      }),
    );

    try {
      await invoke<void>(command, { streamId, ...args });
    } catch (err) {
      cleanup();
      rejectDone(err);
    }
  })();

  return {
    streamId,
    done: donePromise,
    cancel: async () => {
      await invoke("cancel_stream", { streamId });
    },
  };
}

export function sendMessage(
  conversationId: string,
  parentId: string | null,
  userMessageId: string,
  assistantMessageId: string,
  userContent: string,
  onDelta: (text: string) => void,
  onReasoning?: (text: string) => void,
): StreamHandle {
  return startStream(
    [
      "send_message",
      {
        conversationId,
        parentId,
        userMessageId,
        assistantMessageId,
        userContent,
      },
    ],
    onDelta,
    onReasoning,
  );
}

export function regenerate(
  assistantMessageId: string,
  newAssistantMessageId: string,
  onDelta: (text: string) => void,
  onReasoning?: (text: string) => void,
): StreamHandle {
  return startStream(
    ["regenerate", { assistantMessageId, newAssistantMessageId }],
    onDelta,
    onReasoning,
  );
}

// ----- MCP server (Phase C) -----

export interface McpStatus {
  enabled: boolean;
  running: boolean;
  port: number | null;
  token: string | null;
  url: string | null;
}

export const mcpStatus = () => invoke<McpStatus>("mcp_status");
export const mcpStart = () => invoke<McpStatus>("mcp_start");
export const mcpStop = () => invoke<McpStatus>("mcp_stop");
export const mcpRotateToken = () => invoke<McpStatus>("mcp_rotate_token");

export interface McpClient {
  id: string;
  name: string;
  version: string | null;
  client_info: string | null;
  consent_read: boolean | null;
  consent_write: boolean | null;
  first_seen_at: string;
  last_seen_at: string | null;
  revoked_at: string | null;
}

export const mcpListClients = () => invoke<McpClient[]>("mcp_list_clients");
export const mcpSetConsent = (
  clientId: string,
  consentRead: boolean,
  consentWrite: boolean,
) =>
  invoke<void>("mcp_set_consent", {
    clientId,
    consentRead,
    consentWrite,
  });
export const mcpRevokeClient = (clientId: string) =>
  invoke<void>("mcp_revoke_client", { clientId });

export type ProposalKind = "propose" | "correct";
export type ProposalStatus = "pending" | "accepted" | "rejected" | "superseded";

export interface Proposal {
  id: string;
  client_id: string;
  kind: ProposalKind;
  statement: string | null;
  suggested_category: string | null;
  suggested_confidence: number | null;
  reasoning: string | null;
  source: string | null;
  target_belief_id: string | null;
  suggested_status: string | null;
  correction_reason: string | null;
  status: ProposalStatus;
  decided_at: string | null;
  decided_belief_id: string | null;
  created_at: string;
}

export interface AcceptProposalOverride {
  statement?: string;
  category?: string;
  trustClass?: TrustClass;
}

export const mcpListProposals = (status?: ProposalStatus) =>
  invoke<Proposal[]>("mcp_list_proposals", status ? { status } : {});

export const mcpAcceptProposal = (
  proposalId: string,
  override?: AcceptProposalOverride,
) =>
  invoke<string>("mcp_accept_proposal", {
    proposalId,
    statement: override?.statement,
    category: override?.category,
    trustClass: override?.trustClass,
  });

export const mcpRejectProposal = (proposalId: string) =>
  invoke<void>("mcp_reject_proposal", { proposalId });

// ---------- embedding-model swap ----------

/** Pending vec_beliefs dim swap, surfaced at startup so the user can
 *  confirm or dismiss before old vectors are discarded. */
export interface EmbeddingSwapState {
  old_dim: number;
  new_dim: number;
  belief_count: number;
  legacy_table: string;
  detected_at: string;
}

export const getEmbeddingSwapState = () =>
  invoke<EmbeddingSwapState | null>("get_embedding_swap_state");

/** Accept the swap: drop the legacy backup, clear the banner. The
 *  background embed loop will refill vec_beliefs at the new dim. */
export const confirmEmbeddingSwap = () =>
  invoke<void>("confirm_embedding_swap");

/** Dismiss the warning without dropping the legacy archive — the user
 *  can manually recover later by inspecting vec_beliefs_legacy_<dim>. */
export const dismissEmbeddingSwap = () =>
  invoke<void>("dismiss_embedding_swap");

// ---------- Audit chain ----------

export interface AuditHead {
  seq: number;
  event_hash: string;
  ts: string;
}

export interface AuditEntry {
  seq: number;
  ts: string;
  operation: string;
  actor: string;
  content_hash: string;
  prev_hash: string;
  event_hash: string;
  metadata: string | null;
}

export interface VerifyReport {
  checked: number;
  ok: boolean;
  first_failure: [number, string] | null;
  head: AuditHead | null;
}

export interface SignedHead {
  seq: number;
  event_hash: string;
  ts: string;
  head_payload: string;
  signature_hex: string;
  pubkey_hex: string;
}

export const auditChainHead = () =>
  invoke<AuditHead | null>("audit_chain_head");

export const auditChainVerify = () =>
  invoke<VerifyReport>("audit_chain_verify");

export const auditChainRecent = (limit?: number) =>
  invoke<AuditEntry[]>("audit_chain_recent", { limit: limit ?? null });

export const auditChainPubkeyHex = () =>
  invoke<string>("audit_chain_pubkey_hex");

/** Sign the current chain head and write `<data_dir>/audit-head.sig`.
 *  Returns null when the chain is empty (no head to sign). */
export const auditChainSignHead = () =>
  invoke<SignedHead | null>("audit_chain_sign_head");

/** Read `audit-head.sig` from disk and verify it against the local
 *  pubkey. Throws if the file is missing or the signature is bad. */
export const auditChainVerifySignedHead = () =>
  invoke<SignedHead>("audit_chain_verify_signed_head");

// ---------- Ledger export / import ----------

export interface ExportReport {
  path: string;
  bytes: number;
  belief_count: number;
  version_count: number;
  include_embeddings: boolean;
  audit_seq: number;
}

export interface ImportReport {
  belief_count: number;
  version_count: number;
  provenance_count: number;
  merge_count: number;
  embedding_count: number;
  audit_seq: number;
}

export const exportLedger = (path: string, includeEmbeddings: boolean) =>
  invoke<ExportReport>("export_ledger", { path, includeEmbeddings });

export const importLedger = (path: string, replace: boolean) =>
  invoke<ImportReport>("import_ledger", { path, replace });

/** Same as `exportLedger` but attaches an Ed25519 signature using the
 *  local audit key. The verifier needs only the pubkey to confirm
 *  bit-for-bit fidelity later. */
export const exportLedgerSigned = (path: string, includeEmbeddings: boolean) =>
  invoke<ExportReport>("export_ledger_signed", { path, includeEmbeddings });

/** Same as `importLedger` but verifies any embedded signature against
 *  the local pubkey before replaying. Bad signature aborts before any
 *  rows are touched. */
export const importLedgerVerified = (path: string, replace: boolean) =>
  invoke<ImportReport>("import_ledger_verified", { path, replace });
