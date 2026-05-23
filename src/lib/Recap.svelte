<script lang="ts">
  import { onMount } from "svelte";
  import { generateRecap, listRecaps, type RecapResponse } from "./chat";
  import Markdown from "./Markdown.svelte";

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let recap = $state<RecapResponse | null>(null);
  let dates = $state<string[]>([]);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let showOlder = $state(false);

  function todayLocal(): string {
    const d = new Date();
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  }

  function yesterdayLocal(): string {
    const d = new Date();
    d.setDate(d.getDate() - 1);
    const y = d.getFullYear();
    const m = String(d.getMonth() + 1).padStart(2, "0");
    const day = String(d.getDate()).padStart(2, "0");
    return `${y}-${m}-${day}`;
  }

  let activeDate = $derived(recap?.date ?? todayLocal());
  let olderDates = $derived(
    dates.filter((d) => d !== todayLocal() && d !== yesterdayLocal()),
  );

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
  class="w-[520px] shrink-0 border-l border-neutral-200 dark:border-neutral-800 flex flex-col"
  style="background: var(--pal-surface);"
>
  <div
    class="px-5 pt-4 pb-3 border-b border-neutral-200 dark:border-neutral-800 flex items-center justify-between"
  >
    <h2 class="text-base font-semibold tracking-tight">
      Today, in your AI's eyes
    </h2>
    <button
      onclick={onClose}
      class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 text-lg leading-none"
      aria-label="Close"
    >
      ×
    </button>
  </div>

  <!-- Day selector: today / yesterday + optional older. -->
  <div class="px-5 py-2 flex items-center gap-1 text-xs border-b border-neutral-200 dark:border-neutral-800">
    <button
      onclick={() => load(todayLocal())}
      class="px-2 py-0.5 rounded {activeDate === todayLocal()
        ? 'pal-accent-text font-medium'
        : 'text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100'}"
    >
      Today
    </button>
    <button
      onclick={() => load(yesterdayLocal())}
      class="px-2 py-0.5 rounded {activeDate === yesterdayLocal()
        ? 'pal-accent-text font-medium'
        : 'text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100'}"
    >
      Yesterday
    </button>
    {#if olderDates.length > 0}
      <div class="relative">
        <button
          onclick={() => (showOlder = !showOlder)}
          class="px-2 py-0.5 rounded text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        >
          Earlier ▾
        </button>
        {#if showOlder}
          <div
            class="absolute left-0 top-full mt-1 z-20 w-40 max-h-64 overflow-y-auto rounded-md border border-neutral-200 dark:border-neutral-800 bg-white dark:bg-neutral-900 shadow-lg py-1"
          >
            {#each olderDates as d (d)}
              <button
                onclick={() => { showOlder = false; load(d); }}
                class="w-full text-left px-3 py-1 text-xs hover:bg-neutral-100 dark:hover:bg-neutral-800 {activeDate === d ? 'pal-accent-text' : ''}"
              >
                {d}
              </button>
            {/each}
          </div>
        {/if}
      </div>
    {/if}
    <button
      onclick={() => load(activeDate)}
      disabled={loading}
      class="ml-auto text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 disabled:opacity-50"
      title="Regenerate"
      aria-label="Regenerate"
    >
      ↻
    </button>
  </div>

  <div class="flex-1 overflow-y-auto">
    {#if loading && !recap}
      <p class="px-5 py-6 text-sm text-neutral-500">Loading…</p>
    {:else if error}
      <p class="px-5 py-6 text-sm text-rose-500">Failed: {error}</p>
    {:else if recap}
      <div class="px-6 py-5 prose-chat text-[15px] leading-relaxed">
        <Markdown source={recap.markdown} />
      </div>
    {/if}
  </div>
</aside>
