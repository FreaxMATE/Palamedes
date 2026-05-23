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

// ---------- Belief Ledger / audit ----------

export type BeliefStatus =
  | "asserted"
  | "inferred"
  | "corrected"
  | "contested"
  | "expired"
  | "blocked";

export type TrustClass = "asserted" | "inferred" | "hypothesized" | "summary";

export interface AuditBelief {
  id: string;
  statement: string;
  confidence: number;
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
  created_at: string;
}

export interface VersionItem {
  version_num: number;
  statement: string;
  confidence: number;
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
  newConfidence?: number;
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
  a_confidence: number;
  b_id: string;
  b_statement: string;
  b_status: BeliefStatus;
  b_trust_class: TrustClass;
  b_confidence: number;
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
  confidence?: number;
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
    confidence: override?.confidence,
    trustClass: override?.trustClass,
  });

export const mcpRejectProposal = (proposalId: string) =>
  invoke<void>("mcp_reject_proposal", { proposalId });
