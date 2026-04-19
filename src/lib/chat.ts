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

export const renameConversation = (id: string, title: string) =>
  invoke<void>("rename_conversation", { id, title });

export const getMessages = (conversationId: string) =>
  invoke<Message[]>("get_messages", { conversationId });

export const setCurrentLeaf = (conversationId: string, leafId: string | null) =>
  invoke<void>("set_current_leaf", { conversationId, leafId });

export const deepestDescendant = (messageId: string) =>
  invoke<string>("deepest_descendant", { messageId });

export const getSetting = (key: string) =>
  invoke<string | null>("get_setting", { key });

export const setSetting = (key: string, value: string) =>
  invoke<void>("set_setting", { key, value });

// ---------- streaming ----------

export function sendMessage(
  conversationId: string,
  parentId: string | null,
  userContent: string,
  onDelta: (text: string) => void,
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
      await invoke<void>("send_message", {
        streamId,
        conversationId,
        parentId,
        userContent,
      });
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
