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
      Drew from
    </span>
    {#each receipts as r (r.belief_id)}
      <button
        onclick={() => onOpenAudit(r.belief_id)}
        title={r.statement}
        class="text-[11px] px-1.5 py-0.5 rounded-full border pal-border pal-surface pal-dim hover:pal-accent-border hover:pal-accent-soft-bg inline-flex items-center gap-1"
      >
        <span>{trustBadge(r.trust_class)}</span>
        <span class="max-w-[180px] truncate">{trim(r.statement)}</span>
      </button>
    {/each}
  </div>
{/if}
