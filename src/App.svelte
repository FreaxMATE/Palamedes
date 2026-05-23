<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fade, fly } from "svelte/transition";
  import {
    createConversation,
    deleteConversation,
    deepestDescendant,
    getMessages,
    listConversations,
    regenerate,
    sendMessage,
    setBranchTitle,
    setCurrentLeaf,
    type Conversation,
    type Message,
    type StreamHandle,
  } from "./lib/chat";
  import Markdown from "./lib/Markdown.svelte";
  import Sidebar from "./lib/Sidebar.svelte";
  import Settings from "./lib/Settings.svelte";
  import TreePanel from "./lib/TreePanel.svelte";
  import Audit from "./lib/Audit.svelte";
  import Recap from "./lib/Recap.svelte";
  import Graph from "./lib/Graph.svelte";
  import ReceiptChips from "./lib/ReceiptChips.svelte";
  import { ensureModels } from "./lib/modelStore";
  import { themeState } from "./lib/theme.svelte";

  let conversations: Conversation[] = $state([]);
  let activeId: string | null = $state(null);
  let messages: Message[] = $state([]); // all messages in the active conversation
  let currentLeafId: string | null = $state(null);
  let input = $state("");
  let streaming: StreamHandle | null = $state(null);
  let streamingMessageId: string | null = $state(null);
  // Ephemeral chain-of-thought for the most recent stream. Cleared whenever
  // a new stream starts; the panel collapses when content begins arriving.
  // `reasoningMessageId` outlives `streamingMessageId` so the collapsed
  // "Thought for Xs" panel stays attached to its assistant message after
  // the reply finishes.
  let reasoningText = $state("");
  let reasoningMessageId: string | null = $state(null);
  let reasoningStartMs: number | null = $state(null);
  let reasoningElapsedMs: number | null = $state(null);
  // Collapsed by default — the shimmer label + inline tail are enough
  // signal. Click the summary to peek at the full live stream.
  let reasoningOpen = $state(false);
  let reasoningBodyEl: HTMLDivElement | undefined = $state();

  // Last ~200 chars of the reasoning, whitespace collapsed. Rendered
  // inline next to "Thinking…" as a live single-line ticker.
  let reasoningTail = $derived(
    reasoningText.length === 0
      ? ""
      : reasoningText.slice(-200).replace(/\s+/g, " ").trim(),
  );
  let error: string | null = $state(null);
  let showSettings = $state(false);
  let showTree = $state(false);
  let showAudit = $state(false);
  // When a receipt chip is clicked, the audit panel opens scrolled to this
  // belief. Cleared after the audit panel acts on it (one-shot per click).
  let auditTargetBelief: string | null = $state(null);
  let showRecap = $state(false);
  let showGraph = $state(false);
  let scrollEl: HTMLDivElement;
  let textareaEl: HTMLTextAreaElement;
  let scrollPositions = new Map<string, number>();
  let copiedId: string | null = $state(null);
  let editingTitleId: string | null = $state(null);
  let editingTitleValue = $state("");
  let editingMessageId: string | null = $state(null);
  let editingMessageValue = $state("");

  async function commitBranchTitle() {
    if (!editingTitleId) return;
    const id = editingTitleId;
    const value = editingTitleValue.trim();
    editingTitleId = null;
    if (value) {
      await setBranchTitle(id, value);
      if (activeId) messages = await getMessages(activeId);
    }
  }

  // --- Typewriter buffer: steady-rate char release for smooth rendering ---
  let typewriterBuffer = "";
  let typewriterTimer: number | null = null;
  const TYPEWRITER_MS = 12; // ~80 chars/sec — tweakable

  function appendToStreamingMessage(chunk: string) {
    if (!streamingMessageId) return;
    const idx = messages.findIndex((m) => m.id === streamingMessageId);
    if (idx < 0) return;
    messages[idx] = { ...messages[idx], content: messages[idx].content + chunk };
  }

  function onStreamDelta(delta: string) {
    // First content chunk after reasoning: freeze the elapsed counter so
    // the "Thought for Xs" label stops ticking. Independent of whether
    // the panel is open or closed.
    if (
      reasoningText.length > 0 &&
      reasoningElapsedMs === null &&
      reasoningStartMs !== null &&
      messages.find((m) => m.id === streamingMessageId)?.content === ""
    ) {
      reasoningElapsedMs = Date.now() - reasoningStartMs;
    }
    typewriterBuffer += delta;
    if (typewriterTimer === null) {
      typewriterTimer = window.setInterval(() => {
        if (typewriterBuffer.length === 0) return;
        const catchup = Math.max(1, Math.floor(typewriterBuffer.length / 40));
        const toRelease = typewriterBuffer.slice(0, catchup);
        typewriterBuffer = typewriterBuffer.slice(catchup);
        appendToStreamingMessage(toRelease);
      }, TYPEWRITER_MS);
    }
  }

  function onStreamReasoning(delta: string) {
    if (reasoningText.length === 0) {
      reasoningStartMs = Date.now();
      reasoningMessageId = streamingMessageId;
    }
    reasoningText += delta;
    // Auto-tail only when the user has the panel open (peeking at the
    // live stream). Defer until after Svelte flushes the DOM.
    if (reasoningOpen) {
      queueMicrotask(() => {
        if (reasoningBodyEl) {
          reasoningBodyEl.scrollTop = reasoningBodyEl.scrollHeight;
        }
      });
    }
  }

  function resetReasoning() {
    reasoningText = "";
    reasoningMessageId = null;
    reasoningStartMs = null;
    reasoningElapsedMs = null;
    reasoningOpen = false;
  }

  function stopTypewriter() {
    if (typewriterTimer !== null) {
      window.clearInterval(typewriterTimer);
      typewriterTimer = null;
    }
    if (typewriterBuffer.length > 0) {
      appendToStreamingMessage(typewriterBuffer);
      typewriterBuffer = "";
    }
  }

  async function copyMessage(msg: Message) {
    try {
      await navigator.clipboard.writeText(msg.content);
      copiedId = msg.id;
      setTimeout(() => {
        if (copiedId === msg.id) copiedId = null;
      }, 1500);
    } catch {
      // clipboard blocked; ignore
    }
  }

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

  function relativeTime(iso: string): string {
    const now = Date.now();
    const then = new Date(iso).getTime();
    const s = Math.round((now - then) / 1000);
    if (s < 5) return "just now";
    if (s < 60) return `${s}s ago`;
    const m = Math.round(s / 60);
    if (m < 60) return `${m} min ago`;
    const h = Math.round(m / 60);
    if (h < 24) return `${h}h ago`;
    const d = Math.round(h / 24);
    if (d < 30) return `${d}d ago`;
    return new Date(iso).toLocaleDateString();
  }

  function siblingsOf(all: Message[], msg: Message): Message[] {
    return all
      .filter((m) => m.parent_id === msg.parent_id)
      .sort((a, b) => a.created_at.localeCompare(b.created_at));
  }

  let currentPath = $derived(buildPath(messages, currentLeafId));
  let currentPathIds = $derived(new Set(currentPath.map((m) => m.id)));

  // ---------- commands ----------

  // Recap auto-open: once per local day, slide the recap panel in so the
  // user sees yesterday's inferences without having to remember the shortcut.
  // Tracked in localStorage; manual Ctrl+R still works.
  const RECAP_SEEN_KEY = "palamedes-recap-seen-date";

  function todayLocalIso(): string {
    const d = new Date();
    return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
  }

  function maybeAutoOpenRecap() {
    try {
      const seen = localStorage.getItem(RECAP_SEEN_KEY);
      const today = todayLocalIso();
      if (seen !== today) {
        showRecap = true;
        localStorage.setItem(RECAP_SEEN_KEY, today);
      }
    } catch {
      // localStorage may be unavailable; quietly skip.
    }
  }

  onMount(() => {
    // Fire-and-forget the async bootstrap so we can still return a sync
    // cleanup for the keydown listener (Svelte/Tauri requires onMount's
    // return to be the disposer, not a Promise).
    refreshConversations();
    ensureModels().catch(() => {});
    textareaEl?.focus();
    maybeAutoOpenRecap();

    // Global keyboard shortcuts.
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.key === "n") {
        e.preventDefault();
        newConversation();
      } else if (mod && e.key === ",") {
        e.preventDefault();
        showSettings = !showSettings;
      } else if (mod && e.key === "b") {
        e.preventDefault();
        if (activeId) showTree = !showTree;
      } else if (mod && e.key === "m") {
        e.preventDefault();
        showAudit = !showAudit;
      } else if (mod && e.key === "r") {
        e.preventDefault();
        showRecap = !showRecap;
      } else if (mod && e.key === "g") {
        e.preventDefault();
        showGraph = !showGraph;
      } else if (e.key === "Escape") {
        if (showSettings) showSettings = false;
        else if (showGraph) showGraph = false;
        else if (showRecap) showRecap = false;
        else if (showAudit) showAudit = false;
        else if (showTree) showTree = false;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  async function refreshConversations() {
    conversations = await listConversations();
  }

  async function selectConversation(id: string) {
    if (streaming) return;
    if (activeId && scrollEl) scrollPositions.set(activeId, scrollEl.scrollTop);
    activeId = id;
    messages = await getMessages(id);
    const conv = conversations.find((c) => c.id === id);
    currentLeafId = conv?.current_leaf_id ?? null;
    if (!currentLeafId && messages.length > 0) {
      currentLeafId = messages[messages.length - 1].id;
    }
    await tick();
    const saved = scrollPositions.get(id);
    if (saved !== undefined && scrollEl) {
      scrollEl.scrollTop = saved;
    } else {
      scrollEl?.scrollTo({ top: scrollEl.scrollHeight, behavior: "auto" });
    }
    textareaEl?.focus();
  }

  async function newConversation() {
    if (streaming) return;
    if (activeId && scrollEl) scrollPositions.set(activeId, scrollEl.scrollTop);
    activeId = null;
    messages = [];
    currentLeafId = null;
    error = null;
    await tick();
    textareaEl?.focus();
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

  function onScroll() {
    if (activeId && scrollEl) scrollPositions.set(activeId, scrollEl.scrollTop);
  }

  async function scrollToBottom(behavior: "auto" | "smooth" = "auto") {
    await tick();
    scrollEl?.scrollTo({ top: scrollEl.scrollHeight, behavior });
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

  async function runStream(handle: StreamHandle) {
    streaming = handle;
    try {
      await handle.done;
      stopTypewriter();
      // Re-sync from DB. Since message IDs match our optimistic inserts,
      // Svelte reconciles in place — no DOM recreation, no jump.
      if (activeId) messages = await getMessages(activeId);
      conversations = await listConversations();
    } catch (e: any) {
      error = e?.message ?? String(e);
      stopTypewriter();
      if (activeId) messages = await getMessages(activeId);
    } finally {
      streaming = null;
      streamingMessageId = null;
      typewriterBuffer = "";
      await tick();
      textareaEl?.focus();
    }
  }

  function autoResizeTextarea() {
    if (!textareaEl) return;
    textareaEl.style.height = "auto";
    const max = 240; // ~10 rows
    textareaEl.style.height = Math.min(textareaEl.scrollHeight, max) + "px";
  }

  async function regenerateMessage(msg: Message) {
    if (streaming || msg.role !== "assistant" || !activeId || !msg.parent_id) return;
    error = null;

    const newAsstId = crypto.randomUUID();
    const nowIso = new Date().toISOString();
    const optimistic: Message = {
      id: newAsstId,
      conversation_id: activeId,
      parent_id: msg.parent_id,
      role: "assistant",
      content: "",
      branch_title: null,
      created_at: nowIso,
      model: msg.model,
      tokens_in: null,
      tokens_out: null,
      cost_micro_usd: null,
    };
    messages = [...messages, optimistic];
    currentLeafId = newAsstId;
    streamingMessageId = newAsstId;
    await scrollToBottom();

    resetReasoning();
    const handle = regenerate(msg.id, newAsstId, onStreamDelta, onStreamReasoning);
    await runStream(handle);
  }

  function startEditingMessage(msg: Message) {
    if (streaming) return;
    editingMessageId = msg.id;
    editingMessageValue = msg.content;
  }

  async function saveEditedMessage() {
    if (!editingMessageId || !activeId) return;
    const orig = messages.find((m) => m.id === editingMessageId);
    const newContent = editingMessageValue.trim();
    editingMessageId = null;
    editingMessageValue = "";
    if (!orig || orig.role !== "user" || !newContent || newContent === orig.content) return;

    error = null;
    await kickSend(activeId, orig.parent_id, newContent);
  }

  async function kickSend(convId: string, parentId: string | null, text: string) {
    const userId = crypto.randomUUID();
    const asstId = crypto.randomUUID();
    const nowIso = new Date().toISOString();
    const userMsg: Message = {
      id: userId,
      conversation_id: convId,
      parent_id: parentId,
      role: "user",
      content: text,
      branch_title: null,
      created_at: nowIso,
      model: null,
      tokens_in: null,
      tokens_out: null,
      cost_micro_usd: null,
    };
    const asstMsg: Message = {
      id: asstId,
      conversation_id: convId,
      parent_id: userId,
      role: "assistant",
      content: "",
      branch_title: null,
      created_at: nowIso,
      model: null,
      tokens_in: null,
      tokens_out: null,
      cost_micro_usd: null,
    };
    messages = [...messages, userMsg, asstMsg];
    currentLeafId = asstId;
    streamingMessageId = asstId;
    await scrollToBottom();

    resetReasoning();
    const handle = sendMessage(convId, parentId, userId, asstId, text, onStreamDelta, onStreamReasoning);
    await runStream(handle);
  }

  async function send(asNewBranch = false) {
    const text = input.trim();
    if (!text || streaming) return;
    input = "";
    error = null;

    let convId = activeId;
    if (!convId) {
      const title = text.slice(0, 40).replace(/\n/g, " ") || "New chat";
      const conv = await createConversation(title);
      convId = conv.id;
      activeId = conv.id;
      currentLeafId = null;
      await refreshConversations();
    }

    const leafMsg = currentPath.at(-1);
    const parentId = asNewBranch
      ? (leafMsg?.parent_id ?? null)
      : (leafMsg?.id ?? null);

    await kickSend(convId, parentId, text);
  }

  async function cancel() {
    await streaming?.cancel();
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send(e.ctrlKey || e.metaKey);
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
    <div
      class="border-b border-neutral-200 dark:border-neutral-800 px-4 py-2 flex justify-end gap-4"
    >
      <button
        onclick={() => (showAudit = !showAudit)}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        title="Audit what the AI thinks about you (Ctrl+M)"
      >
        🧠 memory
      </button>
      <button
        onclick={() => (showGraph = !showGraph)}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        title="Memory map (Ctrl+G)"
      >
        ✦ map
      </button>
      <button
        onclick={() => (showTree = !showTree)}
        disabled={!activeId}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 disabled:opacity-30"
        title="Branches (Ctrl+B)"
      >
        ⎇ branches
      </button>
      <button
        onclick={() => themeState.toggle()}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        title={themeState.current === "dark" ? "Switch to light" : "Switch to dark"}
      >
        {themeState.current === "dark" ? "☀" : "☾"}
      </button>
    </div>
    <div
      bind:this={scrollEl}
      onscroll={onScroll}
      class="flex-1 overflow-y-auto p-6 space-y-4 scroll-smooth"
    >
      {#if !activeId && currentPath.length === 0}
        <div
          class="h-full flex items-center justify-center"
          in:fly={{ y: 8, duration: 220 }}
        >
          <div class="text-center max-w-md px-6">
            <h2 class="text-2xl font-semibold mb-3 tracking-tight">
              Chat normally.
            </h2>
            <p class="text-sm text-neutral-500 leading-relaxed">
              I'll keep notes on what I learn about you — confidence and all.
              Press <kbd class="kbd">Ctrl</kbd>+<kbd class="kbd">M</kbd>
              any time to audit what's in there.
            </p>
          </div>
        </div>
      {/if}
      {#each currentPath as msg (msg.id)}
        {@const siblings = siblingsOf(messages, msg)}
        {@const idx = siblings.findIndex((s) => s.id === msg.id)}
        <div
          in:fly={{ y: 6, duration: 180 }}
          class="group flex {msg.role === 'user'
            ? 'justify-end'
            : 'justify-start'}"
        >
          <div class="max-w-[75ch]">
            <div
              class="rounded-lg px-4 py-2 break-words
                     {msg.role === 'user'
                       ? 'pal-accent-bg text-white whitespace-pre-wrap'
                       : 'bg-neutral-100 dark:bg-neutral-800'}"
            >
              {#if editingMessageId === msg.id}
                <textarea
                  bind:value={editingMessageValue}
                  onkeydown={(e) => {
                    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                      e.preventDefault();
                      saveEditedMessage();
                    } else if (e.key === "Escape") {
                      editingMessageId = null;
                    }
                  }}
                  class="w-full bg-transparent border-0 resize-y min-h-[6em] focus:outline-none text-white placeholder-white/60"
                  rows="4"
                ></textarea>
                <div class="flex gap-2 mt-1 text-xs">
                  <button
                    onclick={saveEditedMessage}
                    class="rounded bg-white/20 hover:bg-white/30 px-2 py-0.5"
                  >
                    Save (⌘↵)
                  </button>
                  <button
                    onclick={() => (editingMessageId = null)}
                    class="rounded hover:bg-white/20 px-2 py-0.5"
                  >
                    Cancel
                  </button>
                </div>
              {:else if msg.role === "assistant"}
                {@const isThinking = streamingMessageId === msg.id && !msg.content}
                {@const hasReasoning = reasoningMessageId === msg.id && reasoningText.length > 0}
                {@const elapsedMs =
                  reasoningElapsedMs ??
                  (reasoningStartMs !== null ? Date.now() - reasoningStartMs : 0)}
                {#if isThinking || hasReasoning}
                  <details
                    class="reasoning-details mb-1.5 text-xs text-neutral-500 dark:text-neutral-400 border-l-2 border-neutral-300 dark:border-neutral-700 pl-3"
                    bind:open={reasoningOpen}
                  >
                    <summary class="reasoning-summary cursor-pointer select-none flex items-baseline gap-1.5 hover:text-neutral-700 dark:hover:text-neutral-300">
                      {#if isThinking}
                        <span class="thinking-shimmer italic flex-shrink-0">Thinking…</span>
                        {#if reasoningTail}
                          <span
                            class="reasoning-tail"
                            transition:fade={{ duration: 150 }}
                          >· {reasoningTail}</span>
                        {/if}
                      {:else}
                        <span class="italic">Thought for {Math.max(1, Math.round(elapsedMs / 1000))}s</span>
                      {/if}
                    </summary>
                    {#if reasoningText}
                      {#if isThinking}
                        <div class="reasoning-stream-window mt-1.5 relative">
                          <div
                            class="reasoning-stream-body whitespace-pre-wrap italic leading-relaxed opacity-80"
                            bind:this={reasoningBodyEl}
                          >
                            {reasoningText}
                          </div>
                        </div>
                      {:else}
                        <div class="mt-1.5 whitespace-pre-wrap italic leading-relaxed opacity-80">
                          {reasoningText}
                        </div>
                      {/if}
                    {/if}
                  </details>
                {/if}
                {#if msg.content}
                  <Markdown
                    source={msg.content}
                    throttle={streamingMessageId === msg.id}
                  />
                {/if}
                {#if streamingMessageId !== msg.id && msg.content}
                  <ReceiptChips
                    turnId={msg.id}
                    onOpenAudit={(beliefId) => {
                      auditTargetBelief = beliefId;
                      showAudit = true;
                    }}
                  />
                {/if}
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
                {#if editingTitleId === msg.id}
                  <!-- svelte-ignore a11y_autofocus -->
                  <input
                    type="text"
                    bind:value={editingTitleValue}
                    onblur={commitBranchTitle}
                    onkeydown={(e) => {
                      if (e.key === "Enter") { e.preventDefault(); commitBranchTitle(); }
                      else if (e.key === "Escape") { editingTitleId = null; }
                    }}
                    class="bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0 text-xs focus:outline-none focus:ring-1 focus:ring-violet-500"
                    autofocus
                  />
                {:else if msg.branch_title}
                  <button
                    ondblclick={() => {
                      editingTitleId = msg.id;
                      editingTitleValue = msg.branch_title ?? "";
                    }}
                    class="italic hover:text-neutral-900 dark:hover:text-neutral-100 truncate max-w-[20ch]"
                    title="Double-click to rename"
                  >
                    {msg.branch_title}
                  </button>
                {/if}
              {/if}
              <span
                class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 tabular-nums"
                title={new Date(msg.created_at).toLocaleString()}
              >
                {relativeTime(msg.created_at)}
              </span>
              <button
                onclick={() => branchFromHere(msg.id)}
                class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 hover:text-violet-500"
                disabled={!!streaming}
                title="Next message will branch from here"
              >
                ↳ branch
              </button>
              {#if msg.role === "user"}
                <button
                  onclick={() => startEditingMessage(msg)}
                  class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 hover:text-violet-500"
                  disabled={!!streaming}
                  title="Edit message (creates sibling branch)"
                >
                  ✎ edit
                </button>
              {/if}
              {#if msg.role === "assistant"}
                <button
                  onclick={() => regenerateMessage(msg)}
                  class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 hover:text-violet-500"
                  disabled={!!streaming}
                  title="Regenerate (creates sibling with same history)"
                >
                  ↻ retry
                </button>
                <button
                  onclick={() => copyMessage(msg)}
                  class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 hover:text-neutral-900 dark:hover:text-neutral-100"
                  title="Copy"
                >
                  {copiedId === msg.id ? "copied" : "copy"}
                </button>
              {/if}
            </div>
          </div>
        </div>
      {/each}

      {#if error}
        <div class="text-sm text-red-500">Error: {error}</div>
      {/if}
    </div>

    <div
      class="border-t border-neutral-200 dark:border-neutral-800 p-4 flex gap-2 items-stretch"
    >
      <textarea
        bind:this={textareaEl}
        bind:value={input}
        oninput={autoResizeTextarea}
        onkeydown={onKeydown}
        disabled={!!streaming}
        class="flex-1 resize-none rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-900 px-3 py-2 text-sm focus:outline-none focus:ring-2 disabled:opacity-50 transition-all"
        style="min-height: 2.75rem; max-height: 15rem; --tw-ring-color: rgb(var(--pal-accent));"
        rows="2"
        placeholder="Message Palamedes…"
      ></textarea>

      {#if streaming}
        <button
          onclick={cancel}
          class="rounded-md bg-red-500 hover:bg-red-600 text-white px-4 text-sm font-medium self-stretch"
        >
          Stop
        </button>
      {:else}
        <div class="flex flex-col gap-1 w-36">
          <button
            onclick={() => send(true)}
            disabled={!input.trim()}
            title="Send as new branch (Ctrl+Enter) — creates a sibling of the current leaf"
            class="flex-1 rounded-md border pal-accent-border bg-white dark:bg-neutral-900 pal-accent-text
                   hover:pal-accent-soft-bg disabled:opacity-40
                   px-3 text-xs font-medium inline-flex items-center justify-center gap-1"
          >
            <span>↳</span>
            <span>new branch</span>
          </button>
          <button
            onclick={() => send(false)}
            disabled={!input.trim()}
            title="Send (Enter)"
            class="flex-1 rounded-md pal-accent-bg hover:opacity-90 disabled:opacity-40
                   text-white px-3 text-sm font-medium"
          >
            Send
          </button>
        </div>
      {/if}
    </div>
  </main>

  {#if showTree && activeId}
    <TreePanel
      {messages}
      {currentPathIds}
      {currentLeafId}
      onSelect={switchToMessage}
      onClose={() => (showTree = false)}
    />
  {/if}

  {#if showAudit}
    <Audit
      onClose={() => {
        showAudit = false;
        auditTargetBelief = null;
      }}
      targetBelief={auditTargetBelief}
    />
  {/if}

  {#if showRecap}
    <Recap onClose={() => (showRecap = false)} />
  {/if}
</div>

{#if showGraph}
  <Graph onClose={() => (showGraph = false)} />
{/if}

{#if showSettings}
  <Settings onClose={() => (showSettings = false)} />
{/if}
