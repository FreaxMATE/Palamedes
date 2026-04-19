<script lang="ts">
  import { tick } from "svelte";
  import { sendMessage, type ChatMessage, type StreamHandle } from "./lib/chat";
  import Markdown from "./lib/Markdown.svelte";

  let messages: ChatMessage[] = $state([]);
  let input = $state("");
  let streaming: StreamHandle | null = $state(null);
  let error: string | null = $state(null);
  let scrollEl: HTMLDivElement;

  const SYSTEM_PROMPT =
    "You are Palamedes, a thoughtful personal assistant. Be concise, direct, and honest.";

  async function scrollToBottom() {
    await tick();
    scrollEl?.scrollTo({ top: scrollEl.scrollHeight, behavior: "smooth" });
  }

  async function send() {
    const text = input.trim();
    if (!text || streaming) return;
    input = "";
    error = null;

    messages = [...messages, { role: "user", content: text }];
    await scrollToBottom();

    // Start an empty assistant message we'll append deltas into.
    const assistantIndex = messages.length;
    messages = [...messages, { role: "assistant", content: "" }];

    const handle = sendMessage(
      messages.slice(0, assistantIndex), // send only up to and including the user msg
      (delta) => {
        const current = messages[assistantIndex];
        if (current) {
          messages[assistantIndex] = {
            ...current,
            content: current.content + delta,
          };
          messages = messages;
        }
        scrollToBottom();
      },
      SYSTEM_PROMPT,
    );
    streaming = handle;

    try {
      await handle.done;
    } catch (e: any) {
      error = e?.message ?? String(e);
    } finally {
      streaming = null;
    }
  }

  async function cancel() {
    await streaming?.cancel();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }
</script>

<div class="flex h-full">
  <aside
    class="w-64 shrink-0 border-r border-neutral-200 dark:border-neutral-800 p-4"
  >
    <h1 class="text-lg font-semibold">Palamedes</h1>
    <p class="text-sm text-neutral-500 mt-1">in-memory chat (v0)</p>
  </aside>

  <main class="flex-1 flex flex-col min-w-0">
    <div bind:this={scrollEl} class="flex-1 overflow-y-auto p-6 space-y-4">
      {#each messages as msg, i (i)}
        <div class="flex {msg.role === 'user' ? 'justify-end' : 'justify-start'}">
          <div
            class="max-w-[75ch] rounded-lg px-4 py-2 break-words
                   {msg.role === 'user'
                     ? 'bg-violet-500 text-white whitespace-pre-wrap'
                     : 'bg-neutral-100 dark:bg-neutral-800'}"
          >
            {#if msg.role === "assistant"}
              {#if msg.content}
                <Markdown source={msg.content} />
              {:else if streaming}
                <span class="text-neutral-500">…</span>
              {/if}
            {:else}
              {msg.content}
            {/if}
          </div>
        </div>
      {/each}
      {#if error}
        <div class="text-sm text-red-500">Error: {error}</div>
      {/if}
    </div>

    <div
      class="border-t border-neutral-200 dark:border-neutral-800 p-4 flex gap-2 items-end"
    >
      <textarea
        bind:value={input}
        onkeydown={onKeydown}
        disabled={!!streaming}
        class="flex-1 resize-none rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-900 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-violet-500 disabled:opacity-50"
        rows="3"
        placeholder="Message Palamedes… (Enter to send, Shift+Enter for newline)"
      ></textarea>
      {#if streaming}
        <button
          onclick={cancel}
          class="rounded-md bg-red-500 hover:bg-red-600 text-white px-4 py-2 text-sm font-medium"
        >
          Stop
        </button>
      {:else}
        <button
          onclick={send}
          disabled={!input.trim()}
          class="rounded-md bg-violet-500 hover:bg-violet-600 disabled:opacity-50 text-white px-4 py-2 text-sm font-medium"
        >
          Send
        </button>
      {/if}
    </div>
  </main>
</div>
