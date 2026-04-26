<script lang="ts">
  import { onMount } from "svelte";
  import { captureNote, listArtifacts, type Artifact } from "./chat";

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let draft = $state("");
  let saving = $state(false);
  let saved: Artifact | null = $state(null);
  let error: string | null = $state(null);
  let artifacts: Artifact[] = $state([]);

  async function refresh() {
    try {
      artifacts = await listArtifacts();
    } catch {
      // ignore
    }
  }

  onMount(refresh);

  async function save() {
    if (!draft.trim() || saving) return;
    saving = true;
    error = null;
    try {
      saved = await captureNote(draft);
      draft = "";
      await refresh();
      // Briefly highlight the just-saved card.
      setTimeout(() => (saved = null), 3000);
    } catch (e) {
      error = String(e);
    } finally {
      saving = false;
    }
  }

  function onKey(e: KeyboardEvent) {
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      save();
    }
  }

  function fmtDate(iso: string): string {
    return new Date(iso).toLocaleString();
  }
</script>

<aside
  class="w-[460px] shrink-0 border-l border-neutral-200 dark:border-neutral-800 flex flex-col bg-neutral-50 dark:bg-neutral-950"
>
  <div
    class="px-4 py-3 border-b border-neutral-200 dark:border-neutral-800 flex items-center justify-between"
  >
    <div>
      <h2 class="text-sm font-semibold tracking-tight">Capture</h2>
      <p class="text-[11px] text-neutral-500 mt-0.5">
        Paste anything — beliefs are extracted automatically
      </p>
    </div>
    <button
      onclick={onClose}
      class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
      aria-label="Close"
    >
      ×
    </button>
  </div>

  <div class="p-3 border-b border-neutral-200 dark:border-neutral-800 space-y-2">
    <textarea
      bind:value={draft}
      onkeydown={onKey}
      placeholder="Type or paste a note. Ctrl+Enter to capture."
      class="w-full h-32 text-sm px-2 py-1.5 rounded border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-900 resize-none focus:outline-none focus:ring-1 focus:ring-violet-400"
    ></textarea>
    <div class="flex items-center justify-between">
      <span class="text-[11px] text-neutral-500">
        {#if saved}
          ✓ Saved as <strong>{saved.title}</strong> — extraction running…
        {:else if error}
          <span class="text-rose-500">{error}</span>
        {:else}
          {draft.trim().length > 0 ? `${draft.trim().length} chars` : ""}
        {/if}
      </span>
      <button
        onclick={save}
        disabled={saving || draft.trim().length === 0}
        class="text-xs px-3 py-1 rounded bg-violet-500 text-white hover:bg-violet-600 disabled:opacity-50"
      >
        {saving ? "Saving…" : "Capture"}
      </button>
    </div>
  </div>

  <div class="flex-1 overflow-y-auto">
    {#if artifacts.length === 0}
      <p class="p-4 text-sm text-neutral-500">
        No notes yet. Capture one above to get started.
      </p>
    {:else}
      <ul class="divide-y divide-neutral-200 dark:divide-neutral-800">
        {#each artifacts as a (a.id)}
          <li class="px-3 py-2.5">
            <div class="flex items-start justify-between gap-2">
              <div class="flex-1 min-w-0">
                <p class="text-sm font-medium truncate">{a.title ?? "(untitled)"}</p>
                {#if a.content}
                  <p
                    class="mt-0.5 text-xs text-neutral-500 line-clamp-3 whitespace-pre-wrap"
                  >
                    {a.content}
                  </p>
                {/if}
                <p class="mt-1 text-[10px] text-neutral-400">
                  {a.kind} · {fmtDate(a.created_at)}
                </p>
              </div>
            </div>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</aside>
