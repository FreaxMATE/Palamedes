<script lang="ts">
  import { getSetting, setSetting, wipeChats, wipeAllData } from "./chat";
  import { ensureModels, getCachedModels } from "./modelStore";
  import { onMount } from "svelte";
  import { themeState, PALETTES, type Palette } from "./theme.svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

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

    {#if loaded}
      <div class="space-y-5">
        <div>
          <label class="block text-sm font-medium mb-2">Theme</label>
          <div class="grid grid-cols-2 sm:grid-cols-4 gap-2">
            {#each PALETTES as p (p.id)}
              <button
                type="button"
                onclick={() => themeState.setPalette(p.id as Palette)}
                class="text-left rounded-md border p-2 transition-colors
                       {themeState.palette === p.id
                  ? 'pal-accent-border pal-accent-soft-bg'
                  : 'border-neutral-300 dark:border-neutral-700 hover:border-neutral-400 dark:hover:border-neutral-600'}"
              >
                <div class="text-xs font-medium">{p.label}</div>
                <div class="text-[10px] text-neutral-500 mt-0.5 leading-tight">{p.blurb}</div>
              </button>
            {/each}
          </div>
          <p class="text-xs text-neutral-500 mt-2">
            Affects dark mode. Light mode is unchanged across palettes.
            Click <button
              type="button"
              onclick={() => themeState.toggle()}
              class="underline pal-accent-text"
            >toggle mode</button> to test.
          </p>
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
                    Drafts above reinforce instead of inserting.
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
                    Pairs above this surface in ⇌ Merges.
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
