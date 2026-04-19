<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    createConversation,
    deleteConversation,
    deepestDescendant,
    getMessages,
    listConversations,
    sendMessage,
    setCurrentLeaf,
    type Conversation,
    type Message,
    type StreamHandle,
  } from "./lib/chat";
  import Markdown from "./lib/Markdown.svelte";
  import Sidebar from "./lib/Sidebar.svelte";
  import Settings from "./lib/Settings.svelte";

  let conversations: Conversation[] = $state([]);
  let activeId: string | null = $state(null);
  let messages: Message[] = $state([]); // all messages in the active conversation
  let currentLeafId: string | null = $state(null);
  let input = $state("");
  let asNewBranch = $state(false);
  let streaming: StreamHandle | null = $state(null);
  let streamingAssistantContent = $state("");
  let error: string | null = $state(null);
  let showSettings = $state(false);
  let scrollEl: HTMLDivElement;

  // ---------- path + sibling computation ----------

  function buildPath(all: Message[], leafId: string | null): Message[] {
    if (!leafId) return [];
    const byId = new Map(all.map((m) => [m.id, m]));
    const out: Message[] = [];
    let cursor: string | null = leafId;
    while (cursor) {
      const m = byId.get(cursor);
      if (!m) break;
      out.push(m);
      cursor = m.parent_id;
    }
    return out.reverse();
  }

  function siblingsOf(all: Message[], msg: Message): Message[] {
    return all
      .filter((m) => m.parent_id === msg.parent_id)
      .sort((a, b) => a.created_at.localeCompare(b.created_at));
  }

  let currentPath = $derived(buildPath(messages, currentLeafId));

  // ---------- commands ----------

  onMount(async () => {
    await refreshConversations();
  });

  async function refreshConversations() {
    conversations = await listConversations();
  }

  async function selectConversation(id: string) {
    if (streaming) return;
    activeId = id;
    messages = await getMessages(id);
    const conv = conversations.find((c) => c.id === id);
    currentLeafId = conv?.current_leaf_id ?? null;
    // Fallback: if leaf is missing (shouldn't happen), pick newest message.
    if (!currentLeafId && messages.length > 0) {
      currentLeafId = messages[messages.length - 1].id;
    }
    await scrollToBottom();
  }

  async function newConversation() {
    if (streaming) return;
    activeId = null;
    messages = [];
    currentLeafId = null;
    error = null;
  }

  async function onDelete(id: string) {
    await deleteConversation(id);
    if (activeId === id) {
      activeId = null;
      messages = [];
      currentLeafId = null;
    }
    await refreshConversations();
  }

  async function scrollToBottom() {
    await tick();
    scrollEl?.scrollTo({ top: scrollEl.scrollHeight, behavior: "smooth" });
  }

  async function switchToMessage(messageId: string) {
    if (streaming || !activeId) return;
    // Jump to the deepest descendant of this message so we see the full available path.
    const tip = await deepestDescendant(messageId);
    await setCurrentLeaf(activeId, tip);
    currentLeafId = tip;
    await scrollToBottom();
  }

  async function branchFromHere(messageId: string) {
    if (streaming || !activeId) return;
    // Set the leaf to this exact message. Next send will become its (new) child.
    await setCurrentLeaf(activeId, messageId);
    currentLeafId = messageId;
    await scrollToBottom();
  }

  async function send() {
    const text = input.trim();
    if (!text || streaming) return;
    input = "";
    error = null;

    if (!activeId) {
      const title = text.slice(0, 40).replace(/\n/g, " ") || "New chat";
      const conv = await createConversation(title);
      activeId = conv.id;
      currentLeafId = null;
      await refreshConversations();
    }

    // Determine parent for the new user message:
    //   normal  → child of current leaf
    //   branch  → sibling of current leaf (same parent_id)
    const leafMsg = currentPath.at(-1);
    const parentId = asNewBranch
      ? (leafMsg?.parent_id ?? null)
      : (leafMsg?.id ?? null);

    streamingAssistantContent = "";
    const handle = sendMessage(activeId!, parentId, text, (delta) => {
      streamingAssistantContent += delta;
      scrollToBottom();
    });
    streaming = handle;

    try {
      await handle.done;
      messages = await getMessages(activeId!);
      const conv = (await listConversations()).find((c) => c.id === activeId);
      conversations = await listConversations();
      currentLeafId = conv?.current_leaf_id ?? currentLeafId;
    } catch (e: any) {
      error = e?.message ?? String(e);
      if (activeId) messages = await getMessages(activeId);
    } finally {
      streaming = null;
      streamingAssistantContent = "";
      asNewBranch = false;
      await scrollToBottom();
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
  <Sidebar
    {conversations}
    {activeId}
    onSelect={selectConversation}
    onNew={newConversation}
    onDelete={onDelete}
    onOpenSettings={() => (showSettings = true)}
  />

  <main class="flex-1 flex flex-col min-w-0">
    <div bind:this={scrollEl} class="flex-1 overflow-y-auto p-6 space-y-4">
      {#each currentPath as msg (msg.id)}
        {@const siblings = siblingsOf(messages, msg)}
        {@const idx = siblings.findIndex((s) => s.id === msg.id)}
        <div
          class="group flex {msg.role === 'user'
            ? 'justify-end'
            : 'justify-start'}"
        >
          <div class="max-w-[75ch]">
            <div
              class="rounded-lg px-4 py-2 break-words
                     {msg.role === 'user'
                       ? 'bg-violet-500 text-white whitespace-pre-wrap'
                       : 'bg-neutral-100 dark:bg-neutral-800'}"
            >
              {#if msg.role === "assistant"}
                <Markdown source={msg.content} />
              {:else}
                {msg.content}
              {/if}
            </div>

            <div
              class="mt-1 flex items-center gap-2 text-xs text-neutral-500
                     {msg.role === 'user' ? 'justify-end' : 'justify-start'}"
            >
              {#if siblings.length > 1}
                <button
                  onclick={() => {
                    const prev = siblings[(idx - 1 + siblings.length) % siblings.length];
                    switchToMessage(prev.id);
                  }}
                  class="hover:text-neutral-900 dark:hover:text-neutral-100"
                  disabled={!!streaming}
                  aria-label="Previous sibling"
                >
                  ‹
                </button>
                <span>{idx + 1}/{siblings.length}</span>
                <button
                  onclick={() => {
                    const next = siblings[(idx + 1) % siblings.length];
                    switchToMessage(next.id);
                  }}
                  class="hover:text-neutral-900 dark:hover:text-neutral-100"
                  disabled={!!streaming}
                  aria-label="Next sibling"
                >
                  ›
                </button>
              {/if}
              <button
                onclick={() => branchFromHere(msg.id)}
                class="opacity-0 group-hover:opacity-100 hover:text-violet-500"
                disabled={!!streaming}
                title="Next message will branch from here"
              >
                ↳ branch
              </button>
            </div>
          </div>
        </div>
      {/each}

      {#if streaming}
        <div class="flex justify-start">
          <div
            class="max-w-[75ch] rounded-lg px-4 py-2 break-words bg-neutral-100 dark:bg-neutral-800"
          >
            {#if streamingAssistantContent}
              <Markdown source={streamingAssistantContent} />
            {:else}
              <span class="text-neutral-500">…</span>
            {/if}
          </div>
        </div>
      {/if}

      {#if error}
        <div class="text-sm text-red-500">Error: {error}</div>
      {/if}
    </div>

    <div
      class="border-t border-neutral-200 dark:border-neutral-800 p-4 flex flex-col gap-2"
    >
      <label class="flex items-center gap-2 text-xs text-neutral-500 cursor-pointer select-none">
        <input type="checkbox" bind:checked={asNewBranch} disabled={!!streaming} />
        Send as new branch (sibling of current leaf)
      </label>
      <div class="flex gap-2 items-end">
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
    </div>
  </main>
</div>

{#if showSettings}
  <Settings onClose={() => (showSettings = false)} />
{/if}
