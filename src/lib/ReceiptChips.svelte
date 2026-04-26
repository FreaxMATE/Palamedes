<script lang="ts">
  import { onMount } from "svelte";
  import { getReceiptsForTurn, type ReceiptItem, type TrustClass } from "./chat";

  interface Props {
    turnId: string;
    onOpenAudit: (beliefId: string) => void;
  }

  let { turnId, onOpenAudit }: Props = $props();

  let receipts: ReceiptItem[] = $state([]);
  let loaded = $state(false);

  async function load() {
    try {
      receipts = await getReceiptsForTurn(turnId);
    } catch {
      receipts = [];
    } finally {
      loaded = true;
    }
  }

  onMount(load);

  // If turnId changes (rare for an assistant message but possible), refetch.
  $effect(() => {
    turnId; // dependency
    loaded = false;
    load();
  });

  function trustBadge(tc: TrustClass): string {
    switch (tc) {
      case "asserted":
        return "🔒";
      case "inferred":
        return "🧠";
      case "hypothesized":
        return "❓";
      case "summary":
        return "Σ";
    }
  }

  function trim(s: string, max = 60): string {
    if (s.length <= max) return s;
    return s.slice(0, max - 1) + "…";
  }
</script>

{#if loaded && receipts.length > 0}
  <div class="mt-1.5 flex flex-wrap gap-1 items-center">
    <span class="text-[10px] text-neutral-400 uppercase tracking-wider mr-1">
      Memories used
    </span>
    {#each receipts as r (r.belief_id)}
      <button
        onclick={() => onOpenAudit(r.belief_id)}
        title={r.statement}
        class="text-[11px] px-1.5 py-0.5 rounded-full border border-neutral-300 dark:border-neutral-700 bg-white/60 dark:bg-neutral-900/60 text-neutral-700 dark:text-neutral-300 hover:border-violet-400 hover:bg-violet-50 dark:hover:bg-violet-950/40 inline-flex items-center gap-1"
      >
        <span>{trustBadge(r.trust_class)}</span>
        <span class="max-w-[180px] truncate">{trim(r.statement)}</span>
      </button>
    {/each}
  </div>
{/if}
