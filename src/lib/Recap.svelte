<script lang="ts">
  import { onMount } from "svelte";
  import { generateRecap, listRecaps, type RecapResponse } from "./chat";
  import Markdown from "./Markdown.svelte";

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let recap: RecapResponse | null = $state(null);
  let dates: string[] = $state([]);
  let loading = $state(false);
  let error: string | null = $state(null);

  function todayLocal(): string {
    const d = new Date();
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  }

  async function load(date?: string) {
    loading = true;
    error = null;
    try {
      recap = await generateRecap(date);
      dates = await listRecaps();
    } catch (e) {
      error = String(e);
    } finally {
      loading = false;
    }
  }

  onMount(() => load());
</script>

<aside
  class="w-[520px] shrink-0 border-l border-neutral-200 dark:border-neutral-800 flex flex-col bg-neutral-50 dark:bg-neutral-950"
>
  <div
    class="px-4 py-3 border-b border-neutral-200 dark:border-neutral-800 flex items-center justify-between"
  >
    <div>
      <h2 class="text-sm font-semibold tracking-tight">Daily recap</h2>
      <p class="text-[11px] text-neutral-500 mt-0.5">
        What you did today + what the AI inferred
      </p>
    </div>
    <div class="flex items-center gap-2">
      <select
        class="text-[11px] bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0.5"
        value={recap?.date ?? todayLocal()}
        onchange={(e) => load((e.target as HTMLSelectElement).value)}
      >
        {#if !dates.includes(todayLocal())}
          <option value={todayLocal()}>{todayLocal()} (today)</option>
        {/if}
        {#each dates as d (d)}
          <option value={d}>{d}{d === todayLocal() ? " (today)" : ""}</option>
        {/each}
      </select>
      <button
        onclick={() => load(recap?.date)}
        disabled={loading}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 disabled:opacity-50"
        title="Regenerate"
        aria-label="Regenerate"
      >
        ↻
      </button>
      <button
        onclick={onClose}
        class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        aria-label="Close"
      >
        ×
      </button>
    </div>
  </div>

  <div class="flex-1 overflow-y-auto">
    {#if loading && !recap}
      <p class="p-4 text-sm text-neutral-500">Loading…</p>
    {:else if error}
      <p class="p-4 text-sm text-rose-500">Failed: {error}</p>
    {:else if recap}
      <div class="p-4">
        <Markdown source={recap.markdown} />
        <p class="mt-6 text-[11px] text-neutral-400 break-all">
          Saved to <code>{recap.path}</code>
        </p>
      </div>
    {/if}
  </div>
</aside>
