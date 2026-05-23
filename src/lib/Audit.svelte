<script lang="ts">
  import { onMount } from "svelte";
  import {
    listBeliefsAudit,
    getBeliefDetail,
    updateBelief,
    summarizeNow,
    embedUnembeddedBeliefs,
    listMergeCandidates,
    mergeBeliefs,
    type AuditBelief,
    type BeliefDetail,
    type BeliefStatus,
    type TrustClass,
    type MergeCandidate,
  } from "./chat";

  interface Props {
    onClose: () => void;
    /** If set, expand and scroll this belief into view on first load. Used by
     *  receipt-chip click-through from the chat panel. */
    targetBelief?: string | null;
  }
  let { onClose, targetBelief = null }: Props = $props();

  let beliefs: AuditBelief[] = $state([]);
  let loading = $state(true);
  let expandedId: string | null = $state(null);
  let detail: BeliefDetail | null = $state(null);
  let detailLoading = $state(false);
  let busy = $state(false);
  let summarizing = $state(false);
  let summarizeMessage: string | null = $state(null);

  // Merge candidates panel
  let showMerges = $state(false);
  let mergeLoading = $state(false);
  let candidates: MergeCandidate[] = $state([]);
  let mergingPair: string | null = $state(null);

  async function loadCandidates() {
    mergeLoading = true;
    try {
      candidates = await listMergeCandidates();
    } catch (e) {
      summarizeMessage = `Merge scan failed: ${e}`;
    } finally {
      mergeLoading = false;
    }
  }

  async function toggleMerges() {
    showMerges = !showMerges;
    if (showMerges && candidates.length === 0) {
      await loadCandidates();
    }
  }

  async function applyMerge(c: MergeCandidate, keepA: boolean) {
    const pairKey = `${c.a_id}:${c.b_id}`;
    mergingPair = pairKey;
    try {
      await mergeBeliefs({
        keeperId: keepA ? c.a_id : c.b_id,
        absorbedId: keepA ? c.b_id : c.a_id,
      });
      await refresh();
      await loadCandidates();
    } catch (e) {
      summarizeMessage = `Merge failed: ${e}`;
    } finally {
      mergingPair = null;
    }
  }

  // Filters
  type StatusFilter = "active" | "all" | BeliefStatus;
  let statusFilter: StatusFilter = $state("active");
  let trustFilter: "all" | TrustClass = $state("all");
  let categoryFilter = $state("all");
  let sortBy: "recency" | "confidence" = $state("recency");

  // Action state (for inline forms)
  let actionFor: string | null = $state(null);
  let actionKind: "wrong" | "partial" | "forget" | null = $state(null);
  let reasonInput = $state("");
  let refinedStatement = $state("");
  let alsoBlock = $state(false);

  // UI: collapsed toolbar overflow + filter disclosure + per-row extra actions.
  let toolsOpen = $state(false);
  let filtersOpen = $state(false);
  let extrasFor: string | null = $state(null);

  let activeFilterCount = $derived(
    (statusFilter !== "active" ? 1 : 0) +
      (trustFilter !== "all" ? 1 : 0) +
      (categoryFilter !== "all" ? 1 : 0) +
      (sortBy !== "recency" ? 1 : 0),
  );

  async function refresh() {
    loading = true;
    try {
      beliefs = await listBeliefsAudit();
    } finally {
      loading = false;
    }
  }

  onMount(async () => {
    await refresh();
    // Silent embed-backfill: if any belief lacks a vector, embed it in the
    // background. Idempotent — does nothing when all beliefs are embedded,
    // so it's safe to run every panel-mount.
    embedUnembeddedBeliefs().catch(() => {
      // Network/Nebius hiccup — don't surface; user can retry from ⋯ menu later.
    });
    // Silent merge scan: surfaces a count in the ⋯ menu when near-duplicates
    // exist, so the user notices without us nagging them in the chat.
    listMergeCandidates()
      .then((cs) => { candidates = cs; })
      .catch(() => {});
    if (targetBelief) await jumpToBelief(targetBelief);
  });

  // React to a new target belief landing while the panel is already open
  // (e.g. user clicked a receipt chip on a different turn).
  $effect(() => {
    if (!targetBelief) return;
    if (loading) return;
    jumpToBelief(targetBelief);
  });

  async function jumpToBelief(id: string) {
    if (!beliefs.find((b) => b.id === id)) return;
    // Make sure inactive-status filters don't hide the row we're jumping to.
    const target = beliefs.find((b) => b.id === id);
    if (target && (target.status === "expired" || target.status === "blocked")) {
      statusFilter = "all";
    }
    if (expandedId !== id) await toggleExpand(id);
    // Wait for the row + its expanded detail to render, then scroll.
    requestAnimationFrame(() => {
      const el = document.querySelector(`[data-belief-id="${id}"]`);
      if (el) {
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.classList.add("belief-flash");
        setTimeout(() => el.classList.remove("belief-flash"), 1500);
      }
    });
  }

  let categories = $derived.by(() => {
    const s = new Set<string>();
    for (const b of beliefs) if (b.category) s.add(b.category);
    return Array.from(s).sort();
  });

  // Children of each summary (level=0 beliefs whose parent_summary_id matches).
  let childrenBySummary = $derived.by(() => {
    const m = new Map<string, AuditBelief[]>();
    for (const b of beliefs) {
      if (b.parent_summary_id) {
        if (!m.has(b.parent_summary_id)) m.set(b.parent_summary_id, []);
        m.get(b.parent_summary_id)!.push(b);
      }
    }
    return m;
  });

  let visible = $derived.by(() => {
    let xs = beliefs.slice();
    if (statusFilter === "active") {
      xs = xs.filter((b) => b.status !== "expired" && b.status !== "blocked");
    } else if (statusFilter !== "all") {
      xs = xs.filter((b) => b.status === statusFilter);
    }
    if (trustFilter !== "all") xs = xs.filter((b) => b.trust_class === trustFilter);
    if (categoryFilter !== "all") xs = xs.filter((b) => b.category === categoryFilter);
    if (sortBy === "confidence") {
      xs.sort((a, b) => b.confidence - a.confidence);
    } else {
      xs.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
    }
    return xs;
  });

  async function toggleExpand(id: string) {
    if (expandedId === id) {
      expandedId = null;
      detail = null;
      cancelAction();
      return;
    }
    expandedId = id;
    detail = null;
    detailLoading = true;
    cancelAction();
    try {
      detail = await getBeliefDetail(id);
    } finally {
      detailLoading = false;
    }
  }

  function startAction(id: string, kind: "wrong" | "partial" | "forget") {
    actionFor = id;
    actionKind = kind;
    reasonInput = "";
    alsoBlock = false;
    if (kind === "partial") {
      refinedStatement = detail?.belief.statement ?? "";
    }
  }

  function cancelAction() {
    actionFor = null;
    actionKind = null;
    reasonInput = "";
    refinedStatement = "";
    alsoBlock = false;
  }

  async function applyAction() {
    if (!actionFor || !actionKind) return;
    busy = true;
    try {
      if (actionKind === "wrong") {
        await updateBelief({
          id: actionFor,
          newStatus: "corrected",
          reason: reasonInput || "marked wrong",
          blocklistPattern: alsoBlock ? detail?.belief.statement : undefined,
        });
      } else if (actionKind === "partial") {
        await updateBelief({
          id: actionFor,
          newStatus: "contested",
          newStatement: refinedStatement || undefined,
          reason: reasonInput || "partially right",
        });
      } else if (actionKind === "forget") {
        await updateBelief({
          id: actionFor,
          newStatus: "expired",
          reason: reasonInput || "forgotten",
        });
      }
      cancelAction();
      const id = expandedId;
      await refresh();
      if (id) detail = await getBeliefDetail(id);
    } finally {
      busy = false;
    }
  }

  // ✓ correct and 📌 pin: no extra input needed.
  async function correct(id: string) {
    busy = true;
    try {
      await updateBelief({
        id,
        newStatus: "asserted",
        newTrustClass: "asserted",
        reason: "confirmed correct",
      });
      await refresh();
      if (expandedId === id) detail = await getBeliefDetail(id);
    } finally {
      busy = false;
    }
  }

  async function runSummarize() {
    summarizing = true;
    summarizeMessage = null;
    try {
      const r = await summarizeNow();
      const parts = [
        `${r.summaries_created} summaries`,
        `from ${r.beliefs_covered} beliefs`,
        `across ${r.categories_processed} categories`,
      ];
      if (r.overlaps_dropped > 0) {
        parts.push(`${r.overlaps_dropped} overlap${r.overlaps_dropped === 1 ? "" : "s"} dropped`);
      }
      if (r.hallucinations_dropped > 0) {
        parts.push(
          `${r.hallucinations_dropped} hallucinated id${r.hallucinations_dropped === 1 ? "" : "s"} dropped`,
        );
      }
      summarizeMessage = parts.join(" · ");
      if (r.errors.length > 0) {
        summarizeMessage += ` · ${r.errors.length} error(s)`;
        console.error("summarize errors:", r.errors);
      }
      await refresh();
    } catch (e) {
      summarizeMessage = `Failed: ${e}`;
    } finally {
      summarizing = false;
    }
  }

  async function pin(id: string) {
    busy = true;
    try {
      await updateBelief({
        id,
        newStatus: "asserted",
        newTrustClass: "asserted",
        reason: "pinned by user",
      });
      await refresh();
      if (expandedId === id) detail = await getBeliefDetail(id);
    } finally {
      busy = false;
    }
  }

  function confidenceColor(c: number): string {
    if (c >= 0.8) return "bg-emerald-500";
    if (c >= 0.5) return "bg-amber-500";
    return "bg-rose-400";
  }

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

  function statusColor(s: BeliefStatus): string {
    switch (s) {
      case "asserted":
        return "text-emerald-700 dark:text-emerald-400";
      case "inferred":
        return "text-neutral-600 dark:text-neutral-400";
      case "corrected":
        return "text-rose-600 dark:text-rose-400";
      case "contested":
        return "text-amber-600 dark:text-amber-400";
      case "expired":
        return "text-neutral-400 dark:text-neutral-500 line-through";
      case "blocked":
        return "text-neutral-400 dark:text-neutral-500 line-through";
    }
  }

  function fmtDate(iso: string): string {
    const d = new Date(iso);
    return d.toLocaleString();
  }
</script>

<aside
  class="w-[460px] shrink-0 border-l border-neutral-200 dark:border-neutral-800 flex flex-col bg-neutral-50 dark:bg-neutral-950"
>
  <div
    class="px-4 py-3 border-b border-neutral-200 dark:border-neutral-800 flex items-center justify-between"
  >
    <div>
      <h2 class="text-sm font-semibold tracking-tight">Memory audit</h2>
      <p class="text-[11px] text-neutral-500 mt-0.5">
        What the AI thinks about you
      </p>
    </div>
    <div class="flex items-center gap-1 relative">
      <button
        onclick={refresh}
        class="text-sm text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 px-1.5 py-0.5"
        title="Refresh"
        aria-label="Refresh"
      >
        ↻
      </button>
      <button
        onclick={() => (toolsOpen = !toolsOpen)}
        class="text-sm text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 px-1.5 py-0.5"
        title="Tools"
        aria-label="Tools"
        aria-expanded={toolsOpen}
      >
        ⋯
      </button>
      <button
        onclick={onClose}
        class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 px-1.5 py-0.5"
        aria-label="Close"
      >
        ×
      </button>

      {#if toolsOpen}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="absolute right-0 top-full mt-1 z-20 w-52 rounded-md border border-neutral-200 dark:border-neutral-800 bg-white dark:bg-neutral-900 shadow-lg py-1 text-sm"
          onclick={(e) => e.stopPropagation()}
        >
          <button
            onclick={() => { toolsOpen = false; runSummarize(); }}
            disabled={summarizing}
            class="w-full text-left px-3 py-1.5 hover:bg-neutral-100 dark:hover:bg-neutral-800 disabled:opacity-50"
          >
            {summarizing ? "Σ Summarizing…" : "Σ Summarize now"}
          </button>
          <button
            onclick={() => { toolsOpen = false; toggleMerges(); }}
            class="w-full text-left px-3 py-1.5 hover:bg-neutral-100 dark:hover:bg-neutral-800 flex items-center justify-between"
          >
            <span>{showMerges ? "⇌ Close merges" : "⇌ Find merges"}</span>
            {#if !showMerges && candidates.length > 0}
              <span class="text-[10px] px-1.5 py-0.5 rounded-full pal-accent-soft-bg pal-accent-text">
                {candidates.length}
              </span>
            {/if}
          </button>
        </div>
      {/if}
    </div>
  </div>

  {#if summarizeMessage}
    <div
      class="px-3 py-1.5 text-[11px] text-violet-700 dark:text-violet-400 bg-violet-50 dark:bg-violet-950/40 border-b border-violet-200 dark:border-violet-900"
    >
      {summarizeMessage}
    </div>
  {/if}

  <!-- Filter disclosure: collapsed by default. Active filters visible as a count. -->
  <div class="px-3 py-2 border-b border-neutral-200 dark:border-neutral-800">
    <button
      type="button"
      onclick={() => (filtersOpen = !filtersOpen)}
      class="w-full flex items-center justify-between text-[11px] text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
    >
      <span class="inline-flex items-center gap-1">
        <span class="inline-block transition-transform {filtersOpen ? 'rotate-90' : ''}">›</span>
        Filter{activeFilterCount > 0 ? ` · ${activeFilterCount}` : ""}
      </span>
      <span class="tabular-nums">{visible.length} / {beliefs.length}</span>
    </button>

    {#if filtersOpen}
      <div class="mt-2 space-y-2">
        <div class="flex items-center gap-1 flex-wrap">
          {#each ["active", "all", "asserted", "inferred", "corrected", "contested", "expired", "blocked"] as s (s)}
            <button
              class="text-[11px] px-2 py-0.5 rounded-full border
                     {statusFilter === s
                ? 'pal-accent-bg text-white pal-accent-border'
                : 'border-neutral-300 dark:border-neutral-700 text-neutral-600 dark:text-neutral-400 hover:pal-accent-border'}"
              onclick={() => (statusFilter = s as StatusFilter)}
            >
              {s}
            </button>
          {/each}
        </div>
        <div class="flex items-center gap-2 text-[11px] text-neutral-500">
          <label>
            trust:
            <select
              class="ml-1 bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0.5"
              bind:value={trustFilter}
            >
              <option value="all">all</option>
              <option value="asserted">🔒 asserted</option>
              <option value="inferred">🧠 inferred</option>
              <option value="hypothesized">❓ hypothesized</option>
              <option value="summary">Σ summary</option>
            </select>
          </label>
          <label>
            cat:
            <select
              class="ml-1 bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0.5"
              bind:value={categoryFilter}
            >
              <option value="all">all</option>
              {#each categories as c (c)}
                <option value={c}>{c}</option>
              {/each}
            </select>
          </label>
          <label class="ml-auto">
            sort:
            <select
              class="ml-1 bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0.5"
              bind:value={sortBy}
            >
              <option value="recency">recency</option>
              <option value="confidence">confidence</option>
            </select>
          </label>
        </div>
      </div>
    {/if}
  </div>

  <!-- List -->
  <div class="flex-1 overflow-y-auto">
    {#if showMerges}
      <div class="px-3 pt-2 pb-1 flex items-center justify-between text-[11px] text-neutral-500">
        <span>
          {mergeLoading ? "Scanning…" : `${candidates.length} candidate pair(s)`}
        </span>
        <button
          onclick={loadCandidates}
          disabled={mergeLoading}
          class="hover:text-neutral-900 dark:hover:text-neutral-100 disabled:opacity-50"
          title="Re-scan"
        >
          ↻
        </button>
      </div>
      {#if !mergeLoading && candidates.length === 0}
        <p class="p-4 text-sm text-neutral-500">
          No near-duplicates found. Lower
          <code>dedup_suggest_threshold</code> in Settings to surface looser
          matches.
        </p>
      {/if}
      <ul class="divide-y divide-neutral-200 dark:divide-neutral-800">
        {#each candidates as c (c.a_id + c.b_id)}
          {@const pairKey = `${c.a_id}:${c.b_id}`}
          {@const busy = mergingPair === pairKey}
          <li class="px-3 py-3 space-y-2">
            <div class="flex items-center gap-2 text-[11px]">
              <span
                class="px-1.5 py-px rounded {c.tier === 'definite'
                  ? 'bg-rose-100 dark:bg-rose-950 text-rose-800 dark:text-rose-300'
                  : 'bg-amber-100 dark:bg-amber-950 text-amber-800 dark:text-amber-300'}"
              >
                {c.tier}
              </span>
              <span class="font-mono text-neutral-500">cos {c.cosine.toFixed(3)}</span>
            </div>
            <div class="grid grid-cols-1 gap-2">
              <div
                class="p-2 rounded border border-neutral-200 dark:border-neutral-800 bg-white dark:bg-neutral-900"
              >
                <p class="text-sm">{c.a_statement}</p>
                <p class="text-[10px] text-neutral-500 mt-0.5">
                  {c.a_status} · conf {c.a_confidence.toFixed(2)}
                </p>
                <button
                  class="mt-1.5 text-[11px] px-2 py-0.5 rounded pal-accent-bg text-white hover:opacity-90 disabled:opacity-50"
                  disabled={busy}
                  onclick={() => applyMerge(c, true)}
                  title="Keep this; absorb the other"
                >
                  Keep this ↓
                </button>
              </div>
              <div
                class="p-2 rounded border border-neutral-200 dark:border-neutral-800 bg-white dark:bg-neutral-900"
              >
                <p class="text-sm">{c.b_statement}</p>
                <p class="text-[10px] text-neutral-500 mt-0.5">
                  {c.b_status} · conf {c.b_confidence.toFixed(2)}
                </p>
                <button
                  class="mt-1.5 text-[11px] px-2 py-0.5 rounded pal-accent-bg text-white hover:opacity-90 disabled:opacity-50"
                  disabled={busy}
                  onclick={() => applyMerge(c, false)}
                  title="Keep this; absorb the other"
                >
                  Keep this ↑
                </button>
              </div>
            </div>
          </li>
        {/each}
      </ul>
    {:else if loading}
      <p class="p-4 text-sm text-neutral-500">Loading…</p>
    {:else if visible.length === 0}
      <p class="p-4 text-sm text-neutral-500">
        No beliefs match. Chat for a while — extraction runs after each turn.
      </p>
    {:else}
      <ul class="divide-y divide-neutral-200 dark:divide-neutral-800">
        {#each visible as b (b.id)}
          {@const isSummary = b.trust_class === "summary"}
          <li
            data-belief-id={b.id}
            class="px-3 py-2.5 {isSummary
              ? 'bg-violet-50/40 dark:bg-violet-950/20 border-l-2 border-violet-400 dark:border-violet-700'
              : ''}"
          >
            <button
              class="w-full text-left flex items-start gap-2"
              onclick={() => toggleExpand(b.id)}
            >
              <span
                class="mt-0.5 text-base leading-none {isSummary
                  ? 'text-violet-600 dark:text-violet-400 font-bold'
                  : ''}"
              >
                {trustBadge(b.trust_class)}
              </span>
              <span class="flex-1 min-w-0">
                <span class="text-sm {statusColor(b.status)}">{b.statement}</span>
                <span
                  class="mt-1 flex items-center gap-2 text-[11px] text-neutral-500"
                >
                  <span class="inline-flex items-center gap-1">
                    <span
                      class="inline-block w-12 h-1 rounded-full bg-neutral-200 dark:bg-neutral-800 overflow-hidden"
                    >
                      <span
                        class="block h-full {confidenceColor(b.confidence)}"
                        style="width: {Math.round(b.confidence * 100)}%"
                      ></span>
                    </span>
                    <span class="font-mono">{b.confidence.toFixed(2)}</span>
                  </span>
                  {#if b.category}
                    <span class="px-1.5 py-px rounded bg-neutral-200 dark:bg-neutral-800">
                      {b.category}
                    </span>
                  {/if}
                  <span>{b.status}</span>
                  <span>· {b.provenance_count} src</span>
                  {#if b.version_count > 1}
                    <span>· v{b.version_count}</span>
                  {/if}
                  {#if b.reinforced_count > 0}
                    <span
                      class="px-1.5 py-px rounded bg-cyan-100 dark:bg-cyan-950 text-cyan-800 dark:text-cyan-300"
                      title="Reinforced by later turns via dedup"
                    >
                      🔗 {b.reinforced_count}×
                    </span>
                  {/if}
                </span>
              </span>
              <span class="text-neutral-400 text-xs">{expandedId === b.id ? "▾" : "▸"}</span>
            </button>

            {#if expandedId === b.id}
              <div class="mt-3 pl-7 pr-1">
                <!-- Children of this summary, when applicable -->
                {#if isSummary}
                  {@const kids = childrenBySummary.get(b.id) ?? []}
                  {#if kids.length > 0}
                    <div
                      class="mb-3 p-2 rounded border border-violet-200 dark:border-violet-900 bg-white/40 dark:bg-neutral-900/40"
                    >
                      <p class="text-[11px] text-violet-700 dark:text-violet-400 mb-1">
                        Summarizes {kids.length}{" "}
                        {kids.length === 1 ? "belief" : "beliefs"}
                      </p>
                      <ul class="space-y-1">
                        {#each kids as kid (kid.id)}
                          <li class="text-xs flex items-start gap-2">
                            <span class="text-neutral-400">{trustBadge(kid.trust_class)}</span>
                            <span class="flex-1 {statusColor(kid.status)}">{kid.statement}</span>
                            <span class="font-mono text-[10px] text-neutral-500"
                              >{kid.confidence.toFixed(2)}</span
                            >
                          </li>
                        {/each}
                      </ul>
                    </div>
                  {/if}
                {/if}

                <!-- Primary actions: confirm / wrong. Everything else lives behind ⋯. -->
                <div class="flex items-center gap-1 mb-3">
                  <button
                    class="text-xs px-2.5 py-1 rounded border border-emerald-300 dark:border-emerald-800 text-emerald-700 dark:text-emerald-400 hover:bg-emerald-50 dark:hover:bg-emerald-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => correct(b.id)}
                    title="Confirm correct — promotes to asserted"
                  >
                    ✓ correct
                  </button>
                  <button
                    class="text-xs px-2.5 py-1 rounded border border-rose-300 dark:border-rose-800 text-rose-700 dark:text-rose-400 hover:bg-rose-50 dark:hover:bg-rose-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => startAction(b.id, "wrong")}
                  >
                    ✗ wrong
                  </button>
                  <button
                    class="text-xs px-2 py-1 rounded text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 hover:bg-neutral-100 dark:hover:bg-neutral-900 ml-auto"
                    onclick={() => (extrasFor = extrasFor === b.id ? null : b.id)}
                    title="More actions"
                    aria-label="More actions"
                  >
                    ⋯
                  </button>
                </div>
                {#if extrasFor === b.id}
                  <div class="flex items-center gap-1 flex-wrap mb-3 pl-1">
                    <button
                      class="text-xs px-2 py-1 rounded border border-amber-300 dark:border-amber-800 text-amber-700 dark:text-amber-400 hover:bg-amber-50 dark:hover:bg-amber-950 disabled:opacity-50"
                      disabled={busy}
                      onclick={() => { extrasFor = null; startAction(b.id, "partial"); }}
                    >
                      ~ partial
                    </button>
                    <button
                      class="text-xs px-2 py-1 rounded border pal-accent-border pal-accent-text hover:pal-accent-soft-bg disabled:opacity-50"
                      disabled={busy}
                      onclick={() => { extrasFor = null; pin(b.id); }}
                      title="Pin — locks as user-asserted"
                    >
                      📌 pin
                    </button>
                    <button
                      class="text-xs px-2 py-1 rounded border border-neutral-300 dark:border-neutral-700 text-neutral-600 dark:text-neutral-400 hover:bg-neutral-100 dark:hover:bg-neutral-900 disabled:opacity-50"
                      disabled={busy}
                      onclick={() => { extrasFor = null; startAction(b.id, "forget"); }}
                    >
                      🗑 forget
                    </button>
                  </div>
                {/if}

                <!-- Inline action form -->
                {#if actionFor === b.id && actionKind}
                  <div
                    class="mb-3 p-2 rounded border border-neutral-200 dark:border-neutral-800 bg-white dark:bg-neutral-900 space-y-2"
                  >
                    {#if actionKind === "partial"}
                      <label class="block text-[11px] text-neutral-500">
                        Refined statement
                        <input
                          type="text"
                          bind:value={refinedStatement}
                          class="mt-0.5 w-full text-xs px-2 py-1 rounded border border-neutral-300 dark:border-neutral-700 bg-transparent"
                        />
                      </label>
                    {/if}
                    <label class="block text-[11px] text-neutral-500">
                      Reason{actionKind === "wrong" ? " (e.g. 'was just curious')" : ""}
                      <input
                        type="text"
                        bind:value={reasonInput}
                        class="mt-0.5 w-full text-xs px-2 py-1 rounded border border-neutral-300 dark:border-neutral-700 bg-transparent"
                        placeholder={actionKind === "wrong"
                          ? "why this is wrong"
                          : actionKind === "partial"
                            ? "what's right vs wrong"
                            : "why forget"}
                      />
                    </label>
                    {#if actionKind === "wrong"}
                      <label class="flex items-center gap-2 text-[11px] text-neutral-500">
                        <input type="checkbox" bind:checked={alsoBlock} />
                        Also block re-inference (hard-pin wrong)
                      </label>
                    {/if}
                    <div class="flex gap-2 justify-end">
                      <button
                        class="text-xs px-2 py-1 rounded text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
                        onclick={cancelAction}
                        disabled={busy}
                      >
                        Cancel
                      </button>
                      <button
                        class="text-xs px-2 py-1 rounded pal-accent-bg text-white hover:opacity-90 disabled:opacity-50"
                        onclick={applyAction}
                        disabled={busy}
                      >
                        Apply
                      </button>
                    </div>
                  </div>
                {/if}

                <!-- Detail (versions + provenance) -->
                {#if detailLoading}
                  <p class="text-xs text-neutral-500">Loading detail…</p>
                {:else if detail}
                  <div class="space-y-2 text-xs">
                    {#each detail.versions.slice().reverse() as v (v.version_num)}
                      <div
                        class="p-2 rounded border border-neutral-200 dark:border-neutral-800 bg-white/40 dark:bg-neutral-900/40"
                      >
                        <div class="flex items-center justify-between text-[11px] text-neutral-500">
                          <span>v{v.version_num} · {v.editor} · {fmtDate(v.created_at)}</span>
                          <span class="font-mono">{v.confidence.toFixed(2)}</span>
                        </div>
                        <p class="mt-0.5 text-neutral-800 dark:text-neutral-200">
                          {v.statement}
                        </p>
                        {#if v.reason}
                          <p class="mt-0.5 text-[11px] text-neutral-500 italic">
                            "{v.reason}"
                          </p>
                        {/if}
                        {#if v.provenance.length > 0}
                          <ul class="mt-1.5 space-y-1">
                            {#each v.provenance as p (p.source_id + p.relation)}
                              <li
                                class="text-[11px] text-neutral-500 border-l-2 border-violet-300 dark:border-violet-800 pl-2"
                              >
                                <span class="font-medium">{p.relation}</span>
                                ({p.source_type}){#if p.preview}: <span class="italic">{p.preview}</span>{/if}
                              </li>
                            {/each}
                          </ul>
                        {/if}
                      </div>
                    {/each}
                  </div>
                {/if}
              </div>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
  </div>
</aside>
