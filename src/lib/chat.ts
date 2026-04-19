import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type Role = "user" | "assistant" | "system";

export interface ChatMessage {
  role: Role;
  content: string;
}

interface StreamChunk {
  stream_id: string;
  delta: string;
}

interface StreamDone {
  stream_id: string;
}

interface StreamError {
  stream_id: string;
  error: string;
}

export interface StreamHandle {
  streamId: Promise<string>;
  done: Promise<void>;
  cancel: () => Promise<void>;
}

/**
 * Send chat history to the Rust backend and stream the assistant's reply.
 * `onDelta` is called for each text chunk.
 */
export function sendMessage(
  history: ChatMessage[],
  onDelta: (text: string) => void,
  systemPrompt?: string,
): StreamHandle {
  const streamId = crypto.randomUUID();

  let resolveDone!: () => void;
  let rejectDone!: (err: unknown) => void;
  const donePromise = new Promise<void>((res, rej) => {
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
          resolveDone();
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
      await invoke<string>("send_message", {
        streamId,
        history,
        systemPrompt: systemPrompt ?? null,
      });
    } catch (err) {
      cleanup();
      rejectDone(err);
    }
  })();

  return {
    streamId: Promise.resolve(streamId),
    done: donePromise,
    cancel: async () => {
      await invoke("cancel_stream", { streamId });
    },
  };
}
