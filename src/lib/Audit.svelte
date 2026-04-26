<script lang="ts">
  import { onMount } from "svelte";
  import {
    listBeliefsAudit,
    getBeliefDetail,
    updateBelief,
    summarizeNow,
    embedUnembeddedBeliefs,
    type AuditBelief,
    type BeliefDetail,
    type BeliefStatus,
    type TrustClass,
  } from "./chat";

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let beliefs: AuditBelief[] = $state([]);
  let loading = $state(true);
  let expandedId: string | null = $state(null);
  let detail: BeliefDetail | null = $state(null);
  let detailLoading = $state(false);
  let busy = $state(false);
  let summarizing = $state(false);
  let summarizeMessage: string | null = $state(null);
  let embedding = $state(false);

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

  async function refresh() {
    loading = true;
    try {
      beliefs = await listBeliefsAudit();
    } finally {
      loading = false;
    }
  }

  onMount(refresh);

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

  async function runEmbed() {
    embedding = true;
    summarizeMessage = null;
    try {
      const r = await embedUnembeddedBeliefs();
      const parts = [`${r.embedded} embedded`];
      if (r.failed > 0) parts.push(`${r.failed} failed`);
      if (r.first_error) parts.push(`[${r.model}] ${r.first_error}`);
      summarizeMessage = parts.join(" · ");
    } catch (e) {
      summarizeMessage = `Embed failed: ${e}`;
    } finally {
      embedding = false;
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
    <div class="flex items-center gap-2">
      <button
        onclick={runEmbed}
        disabled={embedding}
        class="text-[11px] px-2 py-0.5 rounded border border-cyan-300 dark:border-cyan-800 text-cyan-700 dark:text-cyan-400 hover:bg-cyan-50 dark:hover:bg-cyan-950 disabled:opacity-50"
        title="Embed any beliefs that don't yet have a vector (backfill)"
      >
        {embedding ? "Embedding…" : "⌁ Embed"}
      </button>
      <button
        onclick={runSummarize}
        disabled={summarizing}
        class="text-[11px] px-2 py-0.5 rounded border border-violet-300 dark:border-violet-800 text-violet-700 dark:text-violet-400 hover:bg-violet-50 dark:hover:bg-violet-950 disabled:opacity-50"
        title="Cluster + summarize unsummarized beliefs"
      >
        {summarizing ? "Summarizing…" : "Σ Summarize"}
      </button>
      <button
        onclick={refresh}
        class="text-xs text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100"
        title="Refresh"
        aria-label="Refresh"
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

  {#if summarizeMessage}
    <div
      class="px-3 py-1.5 text-[11px] text-violet-700 dark:text-violet-400 bg-violet-50 dark:bg-violet-950/40 border-b border-violet-200 dark:border-violet-900"
    >
      {summarizeMessage}
    </div>
  {/if}

  <!-- Filters -->
  <div class="px-3 py-2 border-b border-neutral-200 dark:border-neutral-800 space-y-2">
    <div class="flex items-center gap-1 flex-wrap">
      {#each ["active", "all", "asserted", "inferred", "corrected", "contested", "expired", "blocked"] as s (s)}
        <button
          class="text-[11px] px-2 py-0.5 rounded-full border
                 {statusFilter === s
            ? 'bg-violet-500 text-white border-violet-500'
            : 'border-neutral-300 dark:border-neutral-700 text-neutral-600 dark:text-neutral-400 hover:border-violet-400'}"
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
      <label>
        sort:
        <select
          class="ml-1 bg-transparent border border-neutral-300 dark:border-neutral-700 rounded px-1 py-0.5"
          bind:value={sortBy}
        >
          <option value="recency">recency</option>
          <option value="confidence">confidence</option>
        </select>
      </label>
      <span class="ml-auto">{visible.length} / {beliefs.length}</span>
    </div>
  </div>

  <!-- List -->
  <div class="flex-1 overflow-y-auto">
    {#if loading}
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

                <!-- Action buttons -->
                <div class="flex items-center gap-1 flex-wrap mb-3">
                  <button
                    class="text-xs px-2 py-1 rounded border border-emerald-300 dark:border-emerald-800 text-emerald-700 dark:text-emerald-400 hover:bg-emerald-50 dark:hover:bg-emerald-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => correct(b.id)}
                    title="Confirm correct — promotes to asserted"
                  >
                    ✓ correct
                  </button>
                  <button
                    class="text-xs px-2 py-1 rounded border border-rose-300 dark:border-rose-800 text-rose-700 dark:text-rose-400 hover:bg-rose-50 dark:hover:bg-rose-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => startAction(b.id, "wrong")}
                  >
                    ✗ wrong
                  </button>
                  <button
                    class="text-xs px-2 py-1 rounded border border-amber-300 dark:border-amber-800 text-amber-700 dark:text-amber-400 hover:bg-amber-50 dark:hover:bg-amber-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => startAction(b.id, "partial")}
                  >
                    ~ partial
                  </button>
                  <button
                    class="text-xs px-2 py-1 rounded border border-violet-300 dark:border-violet-800 text-violet-700 dark:text-violet-400 hover:bg-violet-50 dark:hover:bg-violet-950 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => pin(b.id)}
                    title="Pin — locks as user-asserted"
                  >
                    📌 pin
                  </button>
                  <button
                    class="text-xs px-2 py-1 rounded border border-neutral-300 dark:border-neutral-700 text-neutral-600 dark:text-neutral-400 hover:bg-neutral-100 dark:hover:bg-neutral-900 disabled:opacity-50"
                    disabled={busy}
                    onclick={() => startAction(b.id, "forget")}
                  >
                    🗑 forget
                  </button>
                </div>

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
                        class="text-xs px-2 py-1 rounded bg-violet-500 text-white hover:bg-violet-600 disabled:opacity-50"
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
