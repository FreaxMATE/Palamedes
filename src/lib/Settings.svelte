<script lang="ts">
  import {
    getSetting, setSetting, wipeChats, wipeAllData,
    mcpStatus, mcpStart, mcpStop, mcpRotateToken,
    mcpListClients, mcpSetConsent, mcpRevokeClient,
    type McpStatus, type McpClient,
  } from "./chat";
  import { ensureModels, getCachedModels } from "./modelStore";
  import { onMount } from "svelte";
  import { themeState } from "./theme.svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  type Tab = "general" | "connections";
  let activeTab: Tab = $state("general");

  let systemPrompt = $state("");
  let model = $state("");
  let embeddingModel = $state("");
  let dedupThreshold = $state("0.85");
  let suggestThreshold = $state("0.70");
  let retrievalMinCosine = $state("0.35");
  let models: string[] = $state([]);
  let modelsError: string | null = $state(null);
  let loaded = $state(false);
  let advancedOpen = $state(false);

  // MCP server state
  let mcp: McpStatus | null = $state(null);
  let mcpClients: McpClient[] = $state([]);
  let mcpBusy = $state(false);
  let mcpError: string | null = $state(null);
  let showToken = $state(false);
  let copiedField: string | null = $state(null);

  async function refreshMcp() {
    try {
      [mcp, mcpClients] = await Promise.all([mcpStatus(), mcpListClients()]);
      mcpError = null;
    } catch (e: any) {
      mcpError = e?.message ?? String(e);
    }
  }

  async function setConsent(
    clientId: string,
    consentRead: boolean,
    consentWrite: boolean,
  ) {
    mcpBusy = true;
    mcpError = null;
    try {
      await mcpSetConsent(clientId, consentRead, consentWrite);
      mcpClients = await mcpListClients();
    } catch (e: any) {
      mcpError = e?.message ?? String(e);
    } finally {
      mcpBusy = false;
    }
  }

  async function revokeClient(clientId: string) {
    mcpBusy = true;
    mcpError = null;
    try {
      await mcpRevokeClient(clientId);
      mcpClients = await mcpListClients();
    } catch (e: any) {
      mcpError = e?.message ?? String(e);
    } finally {
      mcpBusy = false;
    }
  }

  function clientStatusLabel(c: McpClient): { label: string; tone: string } {
    if (c.revoked_at) return { label: "revoked", tone: "neutral" };
    if (c.consent_read === null || c.consent_write === null)
      return { label: "pending", tone: "amber" };
    if (c.consent_read && c.consent_write)
      return { label: "read + write", tone: "emerald" };
    if (c.consent_read) return { label: "read only", tone: "sky" };
    if (c.consent_write) return { label: "write only", tone: "sky" };
    return { label: "denied", tone: "rose" };
  }

  async function toggleMcp() {
    mcpBusy = true;
    mcpError = null;
    try {
      mcp = mcp?.running ? await mcpStop() : await mcpStart();
    } catch (e: any) {
      mcpError = e?.message ?? String(e);
    } finally {
      mcpBusy = false;
    }
  }

  async function rotateToken() {
    mcpBusy = true;
    mcpError = null;
    try {
      mcp = await mcpRotateToken();
    } catch (e: any) {
      mcpError = e?.message ?? String(e);
    } finally {
      mcpBusy = false;
    }
  }

  async function copyToClipboard(value: string, field: string) {
    try {
      await navigator.clipboard.writeText(value);
      copiedField = field;
      window.setTimeout(() => {
        if (copiedField === field) copiedField = null;
      }, 1500);
    } catch (e) {
      // best-effort; ignore.
    }
  }

  function claudeDesktopSnippet(): string {
    // Stdio path: ships in Day 7. Until then, surface a placeholder so
    // Claude Desktop users know it's coming.
    return JSON.stringify(
      {
        mcpServers: {
          palamedes: {
            command: "palamedes-mcp",
            // The shim reads PALAMEDES_MCP_URL + PALAMEDES_MCP_TOKEN.
            env: {
              PALAMEDES_MCP_URL: mcp?.url ?? "http://127.0.0.1:5180/mcp",
              PALAMEDES_MCP_TOKEN: mcp?.token ?? "<token>",
            },
          },
        },
      },
      null,
      2,
    );
  }

  function cursorSnippet(): string {
    return JSON.stringify(
      {
        mcpServers: {
          palamedes: {
            url: mcp?.url ?? "http://127.0.0.1:5180/mcp",
            headers: {
              Authorization: `Bearer ${mcp?.token ?? "<token>"}`,
            },
          },
        },
      },
      null,
      2,
    );
  }

  onMount(async () => {
    // Use cached model list if available — opens instantly.
    const cached = getCachedModels();
    if (cached.models) {
      models = cached.models;
      modelsError = cached.error;
    }
    const [sp, m, em, dt, st, rt] = await Promise.all([
      getSetting("system_prompt"),
      getSetting("model"),
      getSetting("embedding_model"),
      getSetting("dedup_cosine_threshold"),
      getSetting("dedup_suggest_threshold"),
      getSetting("retrieval_min_cosine"),
    ]);
    systemPrompt = sp ?? "";
    model = m ?? "moonshotai/Kimi-K2.5";
    embeddingModel = em ?? "Qwen/Qwen3-Embedding-8B";
    dedupThreshold = dt ?? "0.85";
    suggestThreshold = st ?? "0.70";
    retrievalMinCosine = rt ?? "0.35";
    loaded = true;

    // MCP status — best-effort; the Connections tab handles errors.
    await refreshMcp();

    // Kick off a refresh in the background (cheap if already cached).
    if (!cached.models) {
      try {
        models = await ensureModels();
      } catch (e: any) {
        modelsError = e?.message ?? String(e);
      }
    }
  });

  function clamp01(s: string, fallback: string): string {
    const n = parseFloat(s);
    if (Number.isNaN(n)) return fallback;
    return Math.max(0, Math.min(1, n)).toString();
  }

  // Two-click destructive flow: first click arms, second click executes.
  // Auto-disarms after 4s so an accidental click doesn't sit primed forever.
  let confirmingChats = $state(false);
  let confirmingAll = $state(false);
  let wiping = $state(false);
  let wipeStatus: string | null = $state(null);
  let chatsTimer: number | null = null;
  let allTimer: number | null = null;

  function armChats() {
    confirmingAll = false;
    if (allTimer !== null) { window.clearTimeout(allTimer); allTimer = null; }
    confirmingChats = true;
    if (chatsTimer !== null) window.clearTimeout(chatsTimer);
    chatsTimer = window.setTimeout(() => { confirmingChats = false; }, 4000);
  }
  function armAll() {
    confirmingChats = false;
    if (chatsTimer !== null) { window.clearTimeout(chatsTimer); chatsTimer = null; }
    confirmingAll = true;
    if (allTimer !== null) window.clearTimeout(allTimer);
    allTimer = window.setTimeout(() => { confirmingAll = false; }, 4000);
  }

  async function doWipeChats() {
    wiping = true;
    wipeStatus = null;
    try {
      await wipeChats();
      wipeStatus = "All chats deleted. Reload the window to refresh the sidebar.";
    } catch (e: any) {
      wipeStatus = `Failed: ${e?.message ?? String(e)}`;
    } finally {
      wiping = false;
      confirmingChats = false;
    }
  }

  async function doWipeAll() {
    wiping = true;
    wipeStatus = null;
    try {
      await wipeAllData();
      wipeStatus = "Database wiped. Reload the window to start clean.";
    } catch (e: any) {
      wipeStatus = `Failed: ${e?.message ?? String(e)}`;
    } finally {
      wiping = false;
      confirmingAll = false;
    }
  }

  async function save() {
    await Promise.all([
      setSetting("system_prompt", systemPrompt),
      setSetting("model", model),
      setSetting("embedding_model", embeddingModel),
      setSetting("dedup_cosine_threshold", clamp01(dedupThreshold, "0.85")),
      setSetting("dedup_suggest_threshold", clamp01(suggestThreshold, "0.70")),
      setSetting("retrieval_min_cosine", clamp01(retrievalMinCosine, "0.35")),
    ]);
    onClose();
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
  onclick={onClose}
  onkeydown={(e) => e.key === "Escape" && onClose()}
  role="dialog"
  tabindex="-1"
>
  <div
    class="bg-white dark:bg-neutral-900 rounded-lg shadow-xl w-[600px] max-w-[90vw] p-6 max-h-[80vh] overflow-y-auto"
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
    role="document"
  >
    <h2 class="text-lg font-semibold mb-4">Settings</h2>

    <!-- Tab strip -->
    <div class="flex gap-1 mb-5 border-b border-neutral-200 dark:border-neutral-800">
      <button
        type="button"
        onclick={() => (activeTab = "general")}
        class="px-3 py-1.5 text-sm font-medium -mb-px border-b-2 transition-colors
               {activeTab === 'general'
                 ? 'pal-accent-text pal-accent-border'
                 : 'text-neutral-500 border-transparent hover:text-neutral-900 dark:hover:text-neutral-100'}"
      >
        General
      </button>
      <button
        type="button"
        onclick={() => (activeTab = "connections")}
        class="px-3 py-1.5 text-sm font-medium -mb-px border-b-2 transition-colors
               {activeTab === 'connections'
                 ? 'pal-accent-text pal-accent-border'
                 : 'text-neutral-500 border-transparent hover:text-neutral-900 dark:hover:text-neutral-100'}"
      >
        Connections
        {#if mcp?.running}
          <span
            class="inline-block w-1.5 h-1.5 rounded-full bg-emerald-500 ml-1 align-middle"
            title="MCP server is running"
          ></span>
        {/if}
      </button>
    </div>

    {#if loaded && activeTab === "general"}
      <div class="space-y-5">
        <div>
          <p class="block text-sm font-medium mb-2">Appearance</p>
          <div class="flex gap-2">
            <button
              type="button"
              onclick={() => themeState.set("light")}
              class="flex-1 rounded-md border p-2 text-sm transition-colors
                     {themeState.current === 'light'
                ? 'pal-accent-border pal-accent-soft-bg'
                : 'border-neutral-300 dark:border-neutral-700 hover:border-neutral-400 dark:hover:border-neutral-600'}"
            >
              ☀ Light
            </button>
            <button
              type="button"
              onclick={() => themeState.set("dark")}
              class="flex-1 rounded-md border p-2 text-sm transition-colors
                     {themeState.current === 'dark'
                ? 'pal-accent-border pal-accent-soft-bg'
                : 'border-neutral-300 dark:border-neutral-700 hover:border-neutral-400 dark:hover:border-neutral-600'}"
            >
              ☾ Dark
            </button>
          </div>
        </div>

        <div>
          <label class="block text-sm font-medium mb-1" for="model-select">
            Model
          </label>
          {#if modelsError}
            <input
              id="model-select"
              type="text"
              bind:value={model}
              class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
            />
            <p class="text-xs text-red-500 mt-1">
              Couldn’t load model list: {modelsError}
            </p>
          {:else}
            <select
              id="model-select"
              bind:value={model}
              class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
            >
              {#each models as m (m)}
                <option value={m}>{m}</option>
              {/each}
            </select>
          {/if}
          <p class="text-xs text-neutral-500 mt-1">
            Used for new messages. Existing ones keep the model they were
            generated with.
          </p>
        </div>

        <div>
          <label class="block text-sm font-medium mb-1" for="sys-prompt">
            System prompt
          </label>
          <textarea
            id="sys-prompt"
            bind:value={systemPrompt}
            rows="8"
            class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
          ></textarea>
        </div>

        <div class="border-t border-neutral-200 dark:border-neutral-800 pt-4">
          <button
            type="button"
            onclick={() => (advancedOpen = !advancedOpen)}
            class="text-sm font-semibold flex items-center gap-1.5 text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
          >
            <span class="inline-block transition-transform {advancedOpen ? 'rotate-90' : ''}">›</span>
            Advanced
          </button>
          <p class="text-[11px] text-neutral-500 mt-1 ml-4">
            Embedding model + retrieval/dedup thresholds. Defaults are calibrated;
            change only if you know what you're doing.
          </p>

          {#if advancedOpen}
            <div class="space-y-4 mt-4">
              <div>
                <label class="block text-sm font-medium mb-1" for="embed-model">
                  Embedding model
                </label>
                <input
                  id="embed-model"
                  type="text"
                  bind:value={embeddingModel}
                  class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
                  style="--tw-ring-color: rgb(var(--pal-accent));"
                />
                <p class="text-xs text-neutral-500 mt-1">
                  Used for belief embeddings + retrieval. Changing requires
                  re-embedding (drop <code>vec_beliefs</code>) and the schema's
                  fixed dim must match the model's output (default 4096 for
                  Qwen3-Embedding-8B).
                </p>
              </div>

              <div>
                <label class="block text-sm font-medium mb-1" for="retrieval-thresh">
                  Retrieval min cosine
                </label>
                <input
                  id="retrieval-thresh"
                  type="number"
                  step="0.01"
                  min="0"
                  max="1"
                  bind:value={retrievalMinCosine}
                  class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
                  style="--tw-ring-color: rgb(var(--pal-accent));"
                />
                <p class="text-xs text-neutral-500 mt-1">
                  Beliefs below this cosine are dropped before reaching the chat.
                  Default 0.35.
                </p>
              </div>

              <div class="grid grid-cols-2 gap-3">
                <div>
                  <label class="block text-sm font-medium mb-1" for="dedup-thresh">
                    Auto-merge ≥
                  </label>
                  <input
                    id="dedup-thresh"
                    type="number"
                    step="0.01"
                    min="0"
                    max="1"
                    bind:value={dedupThreshold}
                    class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
                    style="--tw-ring-color: rgb(var(--pal-accent));"
                  />
                  <p class="text-xs text-neutral-500 mt-1">
                    Pairs at or above this merge automatically — new drafts
                    reinforce the existing belief at extraction, and existing
                    duplicates collapse in a background sweep (reversible from
                    the Duplicates tab).
                  </p>
                </div>
                <div>
                  <label class="block text-sm font-medium mb-1" for="suggest-thresh">
                    Suggest merge ≥
                  </label>
                  <input
                    id="suggest-thresh"
                    type="number"
                    step="0.01"
                    min="0"
                    max="1"
                    bind:value={suggestThreshold}
                    class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2"
                    style="--tw-ring-color: rgb(var(--pal-accent));"
                  />
                  <p class="text-xs text-neutral-500 mt-1">
                    Pairs above this (but below auto-merge) surface in the
                    Duplicates tab for one-click review.
                  </p>
                </div>
              </div>
            </div>
          {/if}
        </div>

        <div class="border-t border-red-300/50 dark:border-red-900/50 pt-4">
          <h3 class="text-sm font-semibold mb-1 text-red-600 dark:text-red-400">Danger zone</h3>
          <p class="text-xs text-neutral-500 mb-3">
            Destructive. No undo. Settings on this page are preserved.
          </p>

          <div class="space-y-2">
            <div class="flex items-center justify-between gap-3">
              <div class="text-sm">
                <div class="font-medium">Delete all chats</div>
                <div class="text-xs text-neutral-500">
                  Wipes conversations + messages. Beliefs, embeddings, and the
                  memory map survive. Belief provenance pointing at deleted
                  turns will show no preview.
                </div>
              </div>
              {#if !confirmingChats}
                <button
                  onclick={armChats}
                  disabled={wiping}
                  class="shrink-0 px-3 py-1.5 text-xs rounded-md border border-red-400 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950 disabled:opacity-50"
                >
                  Delete all chats
                </button>
              {:else}
                <button
                  onclick={doWipeChats}
                  disabled={wiping}
                  class="shrink-0 px-3 py-1.5 text-xs rounded-md bg-red-500 hover:bg-red-600 text-white disabled:opacity-50"
                >
                  {wiping ? "Deleting…" : "Confirm — delete chats"}
                </button>
              {/if}
            </div>

            <div class="flex items-center justify-between gap-3">
              <div class="text-sm">
                <div class="font-medium">Wipe entire database</div>
                <div class="text-xs text-neutral-500">
                  Wipes chats, beliefs, versions, provenance, blocklist,
                  embeddings, positions, artifacts, recaps, logs. Settings stay.
                </div>
              </div>
              {#if !confirmingAll}
                <button
                  onclick={armAll}
                  disabled={wiping}
                  class="shrink-0 px-3 py-1.5 text-xs rounded-md border border-red-400 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950 disabled:opacity-50"
                >
                  Wipe everything
                </button>
              {:else}
                <button
                  onclick={doWipeAll}
                  disabled={wiping}
                  class="shrink-0 px-3 py-1.5 text-xs rounded-md bg-red-500 hover:bg-red-600 text-white disabled:opacity-50"
                >
                  {wiping ? "Wiping…" : "Confirm — wipe everything"}
                </button>
              {/if}
            </div>

            {#if wipeStatus}
              <p class="text-xs mt-2 {wipeStatus.startsWith('Failed') ? 'text-red-500' : 'text-emerald-600 dark:text-emerald-400'}">
                {wipeStatus}
              </p>
            {/if}
          </div>
        </div>
      </div>
    {:else if loaded && activeTab === "connections"}
      <div class="space-y-5">
        <div>
          <div class="flex items-center justify-between mb-1">
            <h3 class="text-sm font-semibold">MCP server</h3>
            <span
              class="text-xs font-medium px-2 py-0.5 rounded-full
                     {mcp?.running
                       ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-950 dark:text-emerald-400'
                       : 'bg-neutral-100 text-neutral-600 dark:bg-neutral-800 dark:text-neutral-400'}"
            >
              {mcp?.running ? "● running" : "○ stopped"}
            </span>
          </div>
          <p class="text-xs text-neutral-500 mb-3">
            Expose your Belief Ledger to external AIs (Claude Desktop, Cursor,
            Witsy, Open WebUI) over the Model Context Protocol. Read tools query
            the corpus; write tools land in an audit inbox you review.
            <strong class="text-neutral-700 dark:text-neutral-300">
              Listens on 127.0.0.1 only.
            </strong>
            Off by default — turn it on when you want external AIs to see your
            beliefs.
          </p>

          {#if mcpError}
            <p class="text-xs text-red-500 mb-3">
              {mcpError}
            </p>
          {/if}

          <div class="flex gap-2 mb-4">
            <button
              type="button"
              onclick={toggleMcp}
              disabled={mcpBusy}
              class="px-3 py-1.5 text-sm rounded-md disabled:opacity-50
                     {mcp?.running
                       ? 'border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800'
                       : 'pal-accent-bg hover:opacity-90 text-white'}"
            >
              {mcpBusy ? "…" : mcp?.running ? "Stop server" : "Start server"}
            </button>
            <button
              type="button"
              onclick={rotateToken}
              disabled={mcpBusy}
              class="px-3 py-1.5 text-sm rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800 disabled:opacity-50"
              title="Generate a new token. The server stops if it was running — restart to apply."
            >
              Rotate token
            </button>
          </div>

          {#if mcp?.url}
            <div class="space-y-2 mb-4">
              <div>
                <p class="block text-xs font-medium text-neutral-500 mb-1">
                  URL
                </p>
                <div class="flex gap-2">
                  <code class="flex-1 text-xs rounded-md border border-neutral-300 dark:border-neutral-700 bg-neutral-50 dark:bg-neutral-950 px-2 py-1.5 font-mono">
                    {mcp.url}
                  </code>
                  <button
                    type="button"
                    onclick={() => copyToClipboard(mcp!.url!, "url")}
                    class="px-2 text-xs rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                  >
                    {copiedField === "url" ? "✓" : "Copy"}
                  </button>
                </div>
              </div>
            </div>
          {/if}

          {#if mcp?.token}
            <div class="mb-4">
              <p class="block text-xs font-medium text-neutral-500 mb-1">
                Bearer token
              </p>
              <div class="flex gap-2">
                <code class="flex-1 text-xs rounded-md border border-neutral-300 dark:border-neutral-700 bg-neutral-50 dark:bg-neutral-950 px-2 py-1.5 font-mono truncate">
                  {showToken
                    ? mcp.token
                    : "•".repeat(Math.min(mcp.token.length, 48))}
                </code>
                <button
                  type="button"
                  onclick={() => (showToken = !showToken)}
                  class="px-2 text-xs rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                >
                  {showToken ? "Hide" : "Show"}
                </button>
                <button
                  type="button"
                  onclick={() => copyToClipboard(mcp!.token!, "token")}
                  class="px-2 text-xs rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800"
                >
                  {copiedField === "token" ? "✓" : "Copy"}
                </button>
              </div>
              <p class="text-[11px] text-neutral-500 mt-1">
                Required on every MCP request as
                <code>Authorization: Bearer …</code>. If you ever paste this
                into a screenshot, hit Rotate token.
              </p>
            </div>
          {/if}

          {#if mcpClients.length > 0}
            <div class="border-t border-neutral-200 dark:border-neutral-800 pt-4 mb-4">
              <div class="flex items-center justify-between mb-2">
                <h4 class="text-sm font-semibold">Connected clients</h4>
                <button
                  type="button"
                  onclick={refreshMcp}
                  disabled={mcpBusy}
                  class="text-xs underline text-neutral-500 hover:text-neutral-700 dark:hover:text-neutral-300 disabled:opacity-50"
                >
                  Refresh
                </button>
              </div>
              <p class="text-[11px] text-neutral-500 mb-3">
                AIs that have tried to connect. Pending clients can't call any
                tool until you grant consent. Revoke any you don't recognize.
              </p>
              <ul class="space-y-2">
                {#each mcpClients as c (c.id)}
                  {@const status = clientStatusLabel(c)}
                  <li class="rounded-md border border-neutral-200 dark:border-neutral-800 p-3">
                    <div class="flex items-center justify-between gap-2 mb-2">
                      <div class="min-w-0">
                        <div class="text-sm font-medium truncate">
                          {c.name}{c.version ? ` v${c.version}` : ""}
                        </div>
                        <div class="text-[11px] text-neutral-500">
                          first seen {new Date(c.first_seen_at).toLocaleString()}
                        </div>
                      </div>
                      <span
                        class="text-[11px] font-medium px-2 py-0.5 rounded-full
                               {status.tone === 'emerald'
                                 ? 'bg-emerald-100 text-emerald-700 dark:bg-emerald-950 dark:text-emerald-400'
                                 : status.tone === 'sky'
                                 ? 'bg-sky-100 text-sky-700 dark:bg-sky-950 dark:text-sky-400'
                                 : status.tone === 'amber'
                                 ? 'bg-amber-100 text-amber-700 dark:bg-amber-950 dark:text-amber-400'
                                 : status.tone === 'rose'
                                 ? 'bg-rose-100 text-rose-700 dark:bg-rose-950 dark:text-rose-400'
                                 : 'bg-neutral-100 text-neutral-600 dark:bg-neutral-800 dark:text-neutral-400'}"
                      >
                        {status.label}
                      </span>
                    </div>
                    <div class="flex flex-wrap gap-1.5">
                      <button
                        type="button"
                        disabled={mcpBusy}
                        onclick={() => setConsent(c.id, true, false)}
                        class="text-xs px-2 py-1 rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800 disabled:opacity-50"
                      >Read only</button>
                      <button
                        type="button"
                        disabled={mcpBusy}
                        onclick={() => setConsent(c.id, true, true)}
                        class="text-xs px-2 py-1 rounded-md pal-accent-bg text-white hover:opacity-90 disabled:opacity-50"
                      >Read + write</button>
                      <button
                        type="button"
                        disabled={mcpBusy}
                        onclick={() => setConsent(c.id, false, false)}
                        class="text-xs px-2 py-1 rounded-md border border-neutral-300 dark:border-neutral-700 hover:bg-neutral-100 dark:hover:bg-neutral-800 disabled:opacity-50"
                      >Deny</button>
                      {#if !c.revoked_at}
                        <button
                          type="button"
                          disabled={mcpBusy}
                          onclick={() => revokeClient(c.id)}
                          class="ml-auto text-xs px-2 py-1 rounded-md border border-red-300 text-red-600 dark:text-red-400 hover:bg-red-50 dark:hover:bg-red-950 disabled:opacity-50"
                        >Revoke</button>
                      {/if}
                    </div>
                  </li>
                {/each}
              </ul>
            </div>
          {/if}

          {#if mcp?.url && mcp?.token}
            <div class="border-t border-neutral-200 dark:border-neutral-800 pt-4">
              <h4 class="text-sm font-semibold mb-1">Client config</h4>
              <p class="text-[11px] text-neutral-500 mb-3">
                Drop the snippet for your client into its MCP config. Cursor
                points at the URL directly; Claude Desktop runs the
                <code>palamedes-mcp</code> shim (ships next).
              </p>

              <div class="space-y-3">
                <div>
                  <div class="flex items-center justify-between mb-1">
                    <span class="text-xs font-medium text-neutral-500">Cursor (~/.cursor/mcp.json)</span>
                    <button
                      type="button"
                      onclick={() => copyToClipboard(cursorSnippet(), "cursor")}
                      class="text-xs underline pal-accent-text"
                    >
                      {copiedField === "cursor" ? "Copied" : "Copy"}
                    </button>
                  </div>
                  <pre class="text-[11px] rounded-md border border-neutral-300 dark:border-neutral-700 bg-neutral-50 dark:bg-neutral-950 p-2 overflow-x-auto font-mono">{cursorSnippet()}</pre>
                </div>

                <div>
                  <div class="flex items-center justify-between mb-1">
                    <span class="text-xs font-medium text-neutral-500">Claude Desktop (claude_desktop_config.json)</span>
                    <button
                      type="button"
                      onclick={() => copyToClipboard(claudeDesktopSnippet(), "claude")}
                      class="text-xs underline pal-accent-text"
                    >
                      {copiedField === "claude" ? "Copied" : "Copy"}
                    </button>
                  </div>
                  <pre class="text-[11px] rounded-md border border-neutral-300 dark:border-neutral-700 bg-neutral-50 dark:bg-neutral-950 p-2 overflow-x-auto font-mono">{claudeDesktopSnippet()}</pre>
                </div>
              </div>
            </div>
          {/if}
        </div>
      </div>
    {:else}
      <p class="text-sm text-neutral-500">Loading…</p>
    {/if}

    <div class="flex justify-end gap-2 mt-6">
      <button
        onclick={onClose}
        class="px-3 py-1.5 text-sm rounded-md hover:bg-neutral-100 dark:hover:bg-neutral-800"
      >
        Cancel
      </button>
      <button
        onclick={save}
        disabled={!loaded}
        class="px-3 py-1.5 text-sm rounded-md pal-accent-bg hover:opacity-90 text-white disabled:opacity-50"
      >
        Save
      </button>
    </div>
  </div>
</div>

