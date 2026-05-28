<script lang="ts">
  import { onMount, tick } from "svelte";
  import Icon from "./Icon.svelte";
  import {
    listBeliefsAudit,
    getBeliefDetail,
    updateBelief,
    summarizeNow,
    embedUnembeddedBeliefs,
    listMergeCandidates,
    mergeBeliefs,
    dismissMergeCandidate,
    autoMergeDuplicates,
    mergeAllCandidates,
    recentMerges,
    undoMerge,
    mcpListProposals,
    mcpListClients,
    mcpAcceptProposal,
    mcpRejectProposal,
    type AuditBelief,
    type BeliefDetail,
    type BeliefStatus,
    type TrustClass,
    type MergeCandidate,
    type MergeRecord,
    type Proposal,
    type McpClient,
  } from "./chat";

  interface Props {
    onClose: () => void;
    /** If set, expand and scroll this belief into view on first load. Used by
     *  receipt-chip click-through from the chat panel. */
    targetBelief?: string | null;
    /** Jump to the exact source utterance a belief was extracted from. */
    onOpenSource?: (conversationId: string, messageId: string) => void;
  }
  let { onClose, targetBelief = null, onOpenSource }: Props = $props();

  // Confidence is structural, never self-reported. Shown as a coarse bucket.
  const CONF_TITLE =
    "Structural confidence — how consistently this belief recurs across your " +
    "conversations (reinforcement + recency + how it was sourced). Never " +
    "self-reported by the model.";

  // ----- Data state -----
  let beliefs: AuditBelief[] = $state([]);
  let loading = $state(true);
  let expandedId: string | null = $state(null);
  let detail: BeliefDetail | null = $state(null);
  let detailLoading = $state(false);
  let busy = $state(false);
  let summarizing = $state(false);
  let summarizeMessage: string | null = $state(null);

  // ----- Tools view (replaces ledger) -----
  type ToolsView = "merges" | "inbox";
  let toolsView: ToolsView | null = $state(null);
  let mergeLoading = $state(false);
  let candidates: MergeCandidate[] = $state([]);
  let mergingPair: string | null = $state(null);
  // Duplicate automation: a digest of recent (auto + manual) merges with Undo.
  let merges: MergeRecord[] = $state([]);
  let mergingAll = $state(false);
  let undoingId: string | null = $state(null);

  // ----- Ledger grouping -----
  // Default to collapsed topic cards so the ledger reads as ~6 groups, not
  // dozens of rows. "list" is the flat fallback.
  type GroupView = "topics" | "list";
  let groupView: GroupView = $state("topics");
  let expandedTopics: Set<string> = $state(new Set());

  // ----- Filter chips -----
  type StatusChip = "active" | "all" | BeliefStatus;
  let statusFilter: StatusChip = $state("active");
  let trustFilter: "all" | TrustClass = $state("all");

  // ----- Inline action forms -----
  let actionFor: string | null = $state(null);
  let actionKind: "wrong" | "partial" | "forget" | null = $state(null);
  let reasonInput = $state("");
  let refinedStatement = $state("");
  let alsoBlock = $state(false);

  // ----- MCP inbox -----
  let inbox: Proposal[] = $state([]);
  let inboxClients = $state(new Map<string, McpClient>());
  let inboxBusy: string | null = $state(null);
  let inboxError: string | null = $state(null);

  async function refresh() {
    loading = true;
    try {
      beliefs = await listBeliefsAudit();
    } finally {
      loading = false;
    }
    await refreshInbox();
  }

  async function refreshInbox() {
    try {
      const [proposals, clients] = await Promise.all([
        mcpListProposals("pending"),
        mcpListClients(),
      ]);
      inbox = proposals;
      inboxClients = new Map(clients.map((c) => [c.id, c]));
      inboxError = null;
    } catch (e: any) {
      inboxError = e?.message ?? String(e);
    }
  }

  function clientName(id: string): string {
    return inboxClients.get(id)?.name ?? id.slice(0, 8);
  }

  async function acceptProposal(p: Proposal) {
    inboxBusy = p.id;
    inboxError = null;
    try {
      await mcpAcceptProposal(p.id);
      await refresh();
    } catch (e: any) {
      inboxError = e?.message ?? String(e);
    } finally {
      inboxBusy = null;
    }
  }

  async function rejectProposal(p: Proposal) {
    inboxBusy = p.id;
    inboxError = null;
    try {
      await mcpRejectProposal(p.id);
      await refresh();
    } catch (e: any) {
      inboxError = e?.message ?? String(e);
    } finally {
      inboxBusy = null;
    }
  }

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
      await loadMerges();
    } catch (e) {
      summarizeMessage = `Merge failed: ${e}`;
    } finally {
      mergingPair = null;
    }
  }

  // "These aren't duplicates" — record the pair so it stops resurfacing and
  // is never auto-merged. Drop it locally so the row disappears immediately.
  async function keepSeparate(c: MergeCandidate) {
    const pairKey = `${c.a_id}:${c.b_id}`;
    mergingPair = pairKey;
    try {
      await dismissMergeCandidate(c.a_id, c.b_id);
      candidates = candidates.filter(
        (x) => !(x.a_id === c.a_id && x.b_id === c.b_id),
      );
    } catch (e) {
      summarizeMessage = `Couldn’t keep separate: ${e}`;
    } finally {
      mergingPair = null;
    }
  }

  // One-click batch: merge every surfaced candidate pair (the review tier).
  async function mergeAll() {
    mergingAll = true;
    summarizeMessage = null;
    try {
      const n = await mergeAllCandidates();
      summarizeMessage = n > 0 ? `Merged ${n} duplicate${n === 1 ? "" : "s"}` : "Nothing left to merge";
      await refresh();
      await loadCandidates();
      await loadMerges();
    } catch (e) {
      summarizeMessage = `Merge all failed: ${e}`;
    } finally {
      mergingAll = false;
    }
  }

  // Reverse a merge — restores the absorbed belief and re-indexes it.
  async function undo(m: MergeRecord) {
    undoingId = m.id;
    try {
      await undoMerge(m.id);
      await refresh();
      await loadCandidates();
      await loadMerges();
    } catch (e) {
      summarizeMessage = `Undo failed: ${e}`;
    } finally {
      undoingId = null;
    }
  }

  async function loadMerges() {
    try {
      merges = await recentMerges(20);
    } catch {
      merges = [];
    }
  }

  onMount(async () => {
    await refresh();
    // Background embed-backfill — idempotent.
    embedUnembeddedBeliefs().catch(() => {});
    await loadMerges();
    // Background dedup sweep: collapse the obvious near-duplicates, then
    // refresh the affected views + the candidate badge (now mostly the
    // ambiguous "review" tier).
    autoMergeDuplicates()
      .then(async (n) => {
        if (n > 0) await refresh();
        await loadMerges();
      })
      .catch(() => {})
      .finally(() => {
        listMergeCandidates().then((cs) => { candidates = cs; }).catch(() => {});
      });
    if (targetBelief) await jumpToBelief(targetBelief);
  });

  $effect(() => {
    if (!targetBelief) return;
    if (loading) return;
    jumpToBelief(targetBelief);
  });

  async function jumpToBelief(id: string) {
    if (!beliefs.find((b) => b.id === id)) return;
    const target = beliefs.find((b) => b.id === id);
    if (target && (target.status === "expired" || target.status === "blocked")) {
      statusFilter = "all";
    }
    // In topic mode the row is only in the DOM if its card is open.
    if (target && groupView === "topics") {
      const k = topicKey(target.category);
      if (!expandedTopics.has(k)) {
        const next = new Set(expandedTopics);
        next.add(k);
        expandedTopics = next;
      }
    }
    if (expandedId !== id) await toggleExpand(id);
    await tick();
    requestAnimationFrame(() => {
      const el = document.querySelector(`[data-belief-id="${id}"]`);
      if (el) {
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.classList.add("belief-flash");
        setTimeout(() => el.classList.remove("belief-flash"), 1500);
      }
    });
  }

  let visible = $derived.by(() => {
    let xs = beliefs.slice();
    if (statusFilter === "active") {
      xs = xs.filter((b) => b.status !== "expired" && b.status !== "blocked");
    } else if (statusFilter !== "all") {
      xs = xs.filter((b) => b.status === statusFilter);
    }
    if (trustFilter !== "all") xs = xs.filter((b) => b.trust_class === trustFilter);
    // Recency, newest first
    xs.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
    return xs;
  });

  // Anchors: what the user has stated or confirmed (trust_class "asserted").
  // Shown as a persistent reference strip at the top of the ledger so the
  // high-trust ground truth ("I'm bootstrapping, not VC-backed") stays in view.
  // Only surfaced when the list isn't already narrowed by trust/lifecycle, so
  // every anchor row exists in the list below for jump-to.
  let anchors = $derived.by(() => {
    if (toolsView) return [];
    if (trustFilter !== "all") return [];
    if (statusFilter !== "active" && statusFilter !== "all") return [];
    return beliefs
      .filter((b) => b.trust_class === "asserted" && b.status !== "blocked" && b.status !== "expired")
      .sort((a, b) => b.updated_at.localeCompare(a.updated_at))
      .slice(0, 8);
  });

  // ----- Topic grouping -----
  // Beliefs already carry a coarse `category`; we group on it so the ledger
  // collapses into a handful of topic cards instead of one row per belief.
  const TOPIC_LABELS: Record<string, string> = {
    preference: "Preferences",
    fact: "Facts",
    skill: "Skills",
    plan: "Plans & goals",
    context: "Context",
    other: "Other",
    uncategorized: "Uncategorized",
  };
  // Display order: the things a thinking partner acts on first, then context.
  const TOPIC_ORDER = ["plan", "skill", "preference", "fact", "context", "other", "uncategorized"];

  function topicKey(cat: string | null): string {
    return cat && cat.trim() ? cat.trim() : "uncategorized";
  }
  function topicLabel(k: string): string {
    return TOPIC_LABELS[k] ?? k.charAt(0).toUpperCase() + k.slice(1);
  }

  let topicGroups = $derived.by(() => {
    const m = new Map<string, AuditBelief[]>();
    for (const b of visible) {
      const k = topicKey(b.category);
      if (!m.has(k)) m.set(k, []);
      m.get(k)!.push(b);
    }
    const keys = [...m.keys()].sort((a, b) => {
      const ia = TOPIC_ORDER.indexOf(a);
      const ib = TOPIC_ORDER.indexOf(b);
      const oa = ia < 0 ? 999 : ia;
      const ob = ib < 0 ? 999 : ib;
      if (oa !== ob) return oa - ob;
      return a.localeCompare(b);
    });
    return keys.map((k) => ({ key: k, label: topicLabel(k), items: m.get(k)! }));
  });

  function toggleTopic(k: string) {
    const next = new Set(expandedTopics);
    if (next.has(k)) next.delete(k);
    else next.add(k);
    expandedTopics = next;
  }

  // Count of rows that visually warrant attention in a topic (corrected /
  // contested) so the collapsed card can hint "3 to review".
  function reviewCount(items: AuditBelief[]): number {
    return items.filter((b) => b.status === "corrected" || b.status === "contested").length;
  }

  // Children of each summary
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

  async function runSummarize() {
    summarizing = true;
    summarizeMessage = null;
    try {
      const r = await summarizeNow();
      const parts = [
        `${r.summaries_created} summaries`,
        `from ${r.beliefs_covered} beliefs`,
      ];
      if (r.overlaps_dropped > 0) {
        parts.push(`${r.overlaps_dropped} overlap${r.overlaps_dropped === 1 ? "" : "s"} dropped`);
      }
      summarizeMessage = parts.join(" · ");
      await refresh();
    } catch (e) {
      summarizeMessage = `Failed: ${e}`;
    } finally {
      summarizing = false;
    }
  }

  // ----- Display helpers -----
  function trustGlyph(tc: TrustClass): string {
    switch (tc) {
      case "asserted": return "●";
      case "inferred": return "◐";
      case "hypothesized": return "○";
      case "summary": return "◆";
    }
  }

  /** Tone classes for a row's statement / bar based on status. */
  function statementTone(s: BeliefStatus): string {
    switch (s) {
      case "corrected":
      case "contested":
        return "warn";          // ochre
      case "expired":
      case "blocked":
        return "dim";           // dim + line-through
      default:
        return "ink";           // bone
    }
  }

  /** Compact relative time: 14m, 2h, 3d, 1w, 4mo, 2y */
  function relTime(iso: string): string {
    const t = new Date(iso).getTime();
    if (Number.isNaN(t)) return "—";
    const dSec = Math.max(1, Math.floor((Date.now() - t) / 1000));
    if (dSec < 60) return `${dSec}s`;
    const dMin = Math.floor(dSec / 60);
    if (dMin < 60) return `${dMin}m`;
    const dH = Math.floor(dMin / 60);
    if (dH < 24) return `${dH}h`;
    const dD = Math.floor(dH / 24);
    if (dD < 7) return `${dD}d`;
    const dW = Math.floor(dD / 7);
    if (dW < 5) return `${dW}w`;
    const dMo = Math.floor(dD / 30);
    if (dMo < 12) return `${dMo}mo`;
    return `${Math.floor(dD / 365)}y`;
  }

  function fmtDate(iso: string): string {
    return new Date(iso).toLocaleString();
  }

  // One unified filter row, two dimensions:
  //  - lifecycle chips → belief.status (active/all/corrected/contested/expired/blocked).
  //    "asserted" and "inferred" status values exist in the DB but overlap by name
  //    with trust classes, so we hide them here and rely on the trust glyphs below.
  //  - trust chips → belief.trust_class. Glyph prefix disambiguates from status.
  const STATUS_CHIPS: { id: StatusChip; label: string; title?: string }[] = [
    { id: "active",    label: "active" },
    { id: "all",       label: "all" },
    { id: "corrected", label: "corrected" },
    { id: "contested", label: "contested" },
    { id: "expired",   label: "expired" },
    { id: "blocked",   label: "ruled out", title: "What you've told the AI not to assume — never re-inferred." },
  ];
  const TRUST_CHIPS: { id: TrustClass; label: string; glyph: string }[] = [
    { id: "asserted",     label: "asserted",     glyph: "●" },
    { id: "inferred",     label: "inferred",     glyph: "◐" },
    { id: "hypothesized", label: "hypothesized", glyph: "○" },
    { id: "summary",      label: "summary",      glyph: "◆" },
  ];

  /** Toggle a trust chip: clicking the active one clears back to "all". */
  function toggleTrust(t: TrustClass) {
    trustFilter = trustFilter === t ? "all" : t;
  }

  let toolsBadgeCount = $derived(candidates.length + inbox.length);
</script>

<aside class="audit-aside flex flex-col" style="background: var(--pal-bg-sunken); border-left: 1px solid var(--pal-border); width: 460px;">

  <!-- ============================================================
       HEADER — wordmark · counts · Tools · Close
       ============================================================ -->
  <div class="px-4 py-3 flex items-baseline justify-between" style="border-bottom: 1px solid var(--pal-border);">
    <div class="flex items-baseline gap-3">
      <h2 class="wordmark" style="color: var(--pal-ink);">Memory Ledger</h2>
      <span class="metalabel num" style="color: var(--pal-dim);">{visible.length}/{beliefs.length}</span>
    </div>
    <div class="flex items-center gap-1">
      <button class="audit-icon" onclick={refresh} title="Refresh" aria-label="Refresh">
        <Icon name="refresh" size={14} label="refresh" />
      </button>
      <button
        class="audit-tools-btn"
        onclick={() => (toolsView = toolsView ? null : (inbox.length > 0 ? "inbox" : "merges"))}
        title="Tools — duplicates &amp; inbox"
      >
        Tools
        {#if toolsBadgeCount > 0 && !toolsView}
          <span class="tools-badge num">{toolsBadgeCount}</span>
        {/if}
      </button>
      <button class="audit-icon" onclick={onClose} aria-label="Close">
        <Icon name="close" size={14} label="close" />
      </button>
    </div>
  </div>

  {#if summarizeMessage}
    <div
      class="px-4 py-1.5 text-[11px] pal-accent-text pal-accent-soft-bg"
      style="border-bottom: 1px solid var(--pal-border);"
    >
      {summarizeMessage}
    </div>
  {/if}

  <!-- ============================================================
       FILTER CHIP ROW
       ============================================================ -->
  {#if !toolsView}
    <div class="px-4 py-2 flex items-center gap-1.5 flex-wrap" style="border-bottom: 1px solid var(--pal-border);">
      {#each STATUS_CHIPS as c (c.id)}
        <button
          class="filter-chip"
          class:active={statusFilter === c.id}
          onclick={() => (statusFilter = c.id)}
          title={c.title}
        >
          {c.label}
        </button>
      {/each}
      <span class="chip-divider" aria-hidden="true"></span>
      {#each TRUST_CHIPS as c (c.id)}
        <button
          class="filter-chip"
          class:active={trustFilter === c.id}
          onclick={() => toggleTrust(c.id)}
          title={c.label}
        >
          <span class="glyph" style={c.id === "summary" ? "color: rgb(var(--pal-accent));" : ""}>{c.glyph}</span>
          {c.label}
        </button>
      {/each}
      <div class="seg" style="margin-left:auto;">
        <button class="seg-btn" class:active={groupView === "topics"} onclick={() => (groupView = "topics")} title="Group by topic">Topics</button>
        <button class="seg-btn" class:active={groupView === "list"} onclick={() => (groupView = "list")} title="Flat list">List</button>
      </div>
    </div>
  {/if}

  <!-- ============================================================
       MAIN — Ledger or Tools view
       ============================================================ -->
  <div class="flex-1 overflow-y-auto">

    {#if toolsView}
      <!-- ===================== TOOLS DRAWER ===================== -->
      <div class="px-4 py-3" style="border-bottom: 1px solid var(--pal-border);">
        <div class="flex items-center gap-1.5">
          <button
            class="filter-chip"
            class:active={toolsView === "inbox"}
            onclick={() => (toolsView = "inbox")}
          >
            Inbox
            {#if inbox.length > 0}
              <span class="num" style="margin-left:4px; color: rgb(var(--pal-warn));">{inbox.length}</span>
            {/if}
          </button>
          <button
            class="filter-chip"
            class:active={toolsView === "merges"}
            onclick={() => { toolsView = "merges"; if (candidates.length === 0) loadCandidates(); }}
          >
            Duplicates
            {#if candidates.length > 0}
              <span class="num" style="margin-left:4px; color: rgb(var(--pal-accent));">{candidates.length}</span>
            {/if}
          </button>
          <button
            class="filter-chip"
            disabled={summarizing}
            onclick={runSummarize}
            style="margin-left:auto;"
          >
            {summarizing ? "Grouping…" : "Themes"}
          </button>
        </div>
      </div>

      {#if toolsView === "inbox"}
        <!-- MCP Inbox -->
        {#if inboxError}
          <p class="px-4 py-3 text-xs" style="color: rgb(var(--pal-warn));">{inboxError}</p>
        {/if}
        {#if inbox.length === 0}
          <p class="px-4 py-6 text-sm" style="color: var(--pal-dim);">Inbox is empty. No pending proposals from external AIs.</p>
        {:else}
          <ul>
            {#each inbox as p (p.id)}
              <li class="ledger-row px-4 py-3" style="border-bottom: 1px dotted var(--pal-border);">
                <div class="metalabel mb-1" style="color: rgb(var(--pal-warn));">
                  via MCP · {clientName(p.client_id)} · {p.kind === "propose" ? "new belief" : "correction"}
                </div>
                {#if p.kind === "propose"}
                  <div class="text-sm" style="color: var(--pal-ink);">{p.statement}</div>
                  <div class="text-[11px] mt-1" style="color: var(--pal-dim);">
                    {#if p.suggested_category}<span>category {p.suggested_category} · </span>{/if}
                    {#if p.source}<span>{p.source}</span>{/if}
                  </div>
                  {#if p.reasoning}
                    <div class="text-[11px] mt-1 italic" style="color: var(--pal-dim);">"{p.reasoning}"</div>
                  {/if}
                {:else}
                  <div class="text-sm" style="color: var(--pal-ink);">
                    Mark <span class="num" style="color: var(--pal-dim);">{p.target_belief_id?.slice(0,8)}</span>
                    as <strong>{p.suggested_status}</strong>
                  </div>
                  {#if p.correction_reason}
                    <div class="text-[11px] mt-1 italic" style="color: var(--pal-dim);">"{p.correction_reason}"</div>
                  {/if}
                {/if}
                <div class="mt-2 flex gap-1.5">
                  <button class="row-action accept ic-btn" disabled={inboxBusy === p.id} onclick={() => acceptProposal(p)}>
                    {#if inboxBusy === p.id}…{:else}
                      <Icon name="check" size={11} label="accept" /> accept
                    {/if}
                  </button>
                  <button class="row-action ic-btn" disabled={inboxBusy === p.id} onclick={() => rejectProposal(p)}>
                    <Icon name="x" size={11} label="reject" /> reject
                  </button>
                </div>
              </li>
            {/each}
          </ul>
        {/if}
      {:else}
        <!-- Merges -->
        <p class="px-4 pt-3 pb-1 text-[11px]" style="color: var(--pal-dim); line-height: 1.5;">
          Near-identical beliefs are merged automatically. The pairs below are
          close but ambiguous — review them, or merge them all in one click.
        </p>
        <div class="px-4 py-2 metalabel" style="color: var(--pal-dim); display: flex; align-items: center; gap: 8px; justify-content: space-between;">
          <span>{mergeLoading ? "Scanning…" : `${candidates.length} pair${candidates.length === 1 ? "" : "s"} to review`}</span>
          <div style="display:flex; gap:6px; align-items:center;">
            {#if candidates.length > 0}
              <button class="row-action accent" disabled={mergingAll} onclick={mergeAll}>
                {mergingAll ? "Merging…" : `Merge all (${candidates.length})`}
              </button>
            {/if}
            <button class="audit-icon" onclick={loadCandidates} disabled={mergeLoading} title="Re-scan">
            <Icon name="refresh" size={13} label="re-scan" />
          </button>
          </div>
        </div>
        {#if !mergeLoading && candidates.length === 0}
          <p class="px-4 py-4 text-sm" style="color: var(--pal-dim);">
            Nothing to review — near-duplicates are merged automatically. Lower
            <span class="num">dedup_suggest_threshold</span> in Settings to surface looser matches.
          </p>
        {/if}
        <ul>
          {#each candidates as c (c.a_id + c.b_id)}
            {@const pairKey = `${c.a_id}:${c.b_id}`}
            {@const cBusy = mergingPair === pairKey}
            <li class="px-4 py-3" style="border-bottom: 1px dotted var(--pal-border);">
              <div class="metalabel mb-2" style="display:flex; gap:8px;">
                <span style={c.tier === "definite" ? "color: rgb(var(--pal-warn));" : "color: var(--pal-dim);"}>{c.tier}</span>
                <span class="num" style="color: var(--pal-dim);">cos {c.cosine.toFixed(3)}</span>
              </div>
              <div class="text-sm" style="color: var(--pal-ink);">{c.a_statement}</div>
              <div class="text-[11px] mt-0.5" style="color: var(--pal-dim);">
                <span>{c.a_status}</span> · <span>{c.a_trust_class}</span>
              </div>
              <button class="row-action accent mt-1" disabled={cBusy} onclick={() => applyMerge(c, true)}>Keep this ↓</button>
              <div class="my-2" style="border-top: 1px dashed var(--pal-border);"></div>
              <div class="text-sm" style="color: var(--pal-ink);">{c.b_statement}</div>
              <div class="text-[11px] mt-0.5" style="color: var(--pal-dim);">
                <span>{c.b_status}</span> · <span>{c.b_trust_class}</span>
              </div>
              <button class="row-action accent mt-1" disabled={cBusy} onclick={() => applyMerge(c, false)}>Keep this ↑</button>
              <div class="my-2" style="border-top: 1px dotted var(--pal-border);"></div>
              <button class="row-action" disabled={cBusy} onclick={() => keepSeparate(c)} title="Not a duplicate — keep both and stop suggesting this pair">
                Keep separate — not a duplicate
              </button>
            </li>
          {/each}
        </ul>

        <!-- Recently merged digest with Undo -->
        {#if merges.length > 0}
          <div class="px-4 pt-3 pb-1 metalabel" style="color: var(--pal-dim); border-top: 1px solid var(--pal-border);">
            Recently merged
          </div>
          <ul>
            {#each merges as m (m.id)}
              {@const mBusy = undoingId === m.id}
              <li class="merge-rec px-4 py-2.5" style="border-bottom: 1px dotted var(--pal-border);">
                <div class="metalabel" style="display:flex; gap:8px; align-items:center; margin-bottom:3px;">
                  <span style={m.kind === "auto" ? "color: var(--pal-dim);" : "color: rgb(var(--pal-accent));"}>{m.kind === "auto" ? "auto" : "manual"}</span>
                  {#if m.cosine != null}<span class="num" style="color: var(--pal-dim);">cos {m.cosine.toFixed(3)}</span>{/if}
                </div>
                <div class="text-[12px]" style="color: var(--pal-ink);">{m.keeper_statement}</div>
                <div class="text-[11px] mt-0.5" style="color: var(--pal-dim);">
                  absorbed: <span style="text-decoration: line-through;">{m.absorbed_statement}</span>
                </div>
                <button class="row-action ic-btn mt-1" disabled={mBusy} onclick={() => undo(m)}>
                  {#if mBusy}…{:else}
                    <Icon name="undo" size={11} label="undo" /> undo
                  {/if}
                </button>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}

    {:else if loading}
      <p class="px-4 py-6 text-sm" style="color: var(--pal-dim);">Loading ledger…</p>

    {:else if visible.length === 0}
      <p class="px-4 py-6 text-sm" style="color: var(--pal-dim);">
        No beliefs match. Chat for a while — extraction runs after each turn.
      </p>

    {:else}
      <!-- ===================== ANCHORS ===================== -->
      {#if anchors.length > 0}
        <div class="anchors">
          <div class="metalabel anchors-head">Anchors · what you've stated or confirmed</div>
          <ul>
            {#each anchors as a (a.id)}
              <li>
                <button class="anchor-row" onclick={() => jumpToBelief(a.id)} title="Jump to this belief">
                  <span class="trust-glyph">{trustGlyph(a.trust_class)}</span>
                  <span class="anchor-stmt">{a.statement}</span>
                  <span class="conf-chip conf-{a.confidence_bucket}">{a.confidence_bucket}</span>
                </button>
              </li>
            {/each}
          </ul>
        </div>
      {/if}

      <!-- ===================== BELIEF ROW (snippet) ===================== -->
      {#snippet beliefRow(b: AuditBelief, idx: number)}
          {@const isSummary = b.trust_class === "summary"}
          {@const tone = statementTone(b.status)}
          {@const isExpanded = expandedId === b.id}
          <li
            data-belief-id={b.id}
            class="ledger-row"
            class:expanded={isExpanded}
            class:summary={isSummary}
            class:warn={tone === "warn"}
            class:dim={tone === "dim"}
          >
            <!-- Row -->
            <button
              type="button"
              class="ledger-row-button"
              onclick={() => toggleExpand(b.id)}
            >
              <span class="col-n num">{(idx + 1).toString().padStart(2, "0")}</span>
              <span class="col-stmt">
                <span class="trust-glyph" style={isSummary ? "color: rgb(var(--pal-accent));" : ""}>{trustGlyph(b.trust_class)}</span>
                <span class="stmt-text">{b.statement}</span>
              </span>
              <span class="col-conf" title={CONF_TITLE}>
                <span class="conf-chip conf-{b.confidence_bucket}">{b.confidence_bucket}</span>
              </span>
              <span class="col-cites num">{b.provenance_count}</span>
              <span class="col-time num" title={fmtDate(b.updated_at)}>{relTime(b.updated_at)}</span>
            </button>

            <!-- Expanded detail -->
            {#if isExpanded}
              <div class="expand">
                <!-- Children of a summary, if applicable -->
                {#if isSummary}
                  {@const kids = childrenBySummary.get(b.id) ?? []}
                  {#if kids.length > 0}
                    <div class="children">
                      <div class="metalabel" style="margin-bottom: 6px;">Summarises {kids.length} {kids.length === 1 ? "belief" : "beliefs"}</div>
                      <ul>
                        {#each kids as kid (kid.id)}
                          <li>
                            <span class="trust-glyph" style="color: var(--pal-dim);">{trustGlyph(kid.trust_class)}</span>
                            <span class="flex-1" style="color: {statementTone(kid.status) === 'warn' ? 'rgb(var(--pal-warn))' : 'var(--pal-ink-soft, var(--pal-ink))'};">{kid.statement}</span>
                            <span class="conf-chip conf-{kid.confidence_bucket}" title={CONF_TITLE}>{kid.confidence_bucket}</span>
                          </li>
                        {/each}
                      </ul>
                    </div>
                  {/if}
                {/if}

                <!-- Action buttons -->
                <div class="actions">
                  <button class="row-action accept ic-btn" disabled={busy} onclick={() => correct(b.id)} title="Confirm correct — promotes to asserted">
                    <Icon name="check" size={11} label="confirm" /> correct
                  </button>
                  <button class="row-action warn-action ic-btn" disabled={busy} onclick={() => startAction(b.id, "wrong")}>
                    <Icon name="x" size={11} label="wrong" /> wrong
                  </button>
                  <button class="row-action" disabled={busy} onclick={() => startAction(b.id, "partial")}>~ partial</button>
                  <button class="row-action accent ic-btn" disabled={busy} onclick={() => pin(b.id)} title="Pin — locks as user-asserted">
                    <Icon name="pin" size={11} label="pin" /> pin
                  </button>
                  <button class="row-action" disabled={busy} onclick={() => startAction(b.id, "forget")}>forget</button>
                </div>

                <!-- Action form -->
                {#if actionFor === b.id && actionKind}
                  <div class="action-form">
                    {#if actionKind === "partial"}
                      <label class="metalabel">Refined statement
                        <input type="text" bind:value={refinedStatement} class="action-input" />
                      </label>
                    {/if}
                    <label class="metalabel">Reason
                      <input
                        type="text"
                        bind:value={reasonInput}
                        class="action-input"
                        placeholder={actionKind === "wrong" ? "why this is wrong" : actionKind === "partial" ? "what's right vs wrong" : "why forget"}
                      />
                    </label>
                    {#if actionKind === "wrong"}
                      <label class="action-check">
                        <input type="checkbox" bind:checked={alsoBlock} />
                        <span>Also block re-inference (hard-pin wrong)</span>
                      </label>
                    {/if}
                    <div class="action-form-foot">
                      <button class="row-action" onclick={cancelAction} disabled={busy}>Cancel</button>
                      <button class="row-action accent" onclick={applyAction} disabled={busy}>Apply</button>
                    </div>
                  </div>
                {/if}

                <!-- Versions + provenance -->
                {#if detailLoading}
                  <p class="metalabel" style="margin-top: 12px;">Loading detail…</p>
                {:else if detail}
                  <div class="versions">
                    {#each detail.versions.slice().reverse() as v (v.version_num)}
                      <div class="version">
                        <div class="version-head metalabel">
                          <span>v{v.version_num} · {v.editor} · {fmtDate(v.created_at)}</span>
                        </div>
                        <p class="version-stmt">{v.statement}</p>
                        {#if v.reason}
                          <p class="version-reason" title="The exact words this belief was extracted from">"{v.reason}"</p>
                        {/if}
                        {#if v.provenance.length > 0}
                          <ul class="provenance">
                            {#each v.provenance as p (p.source_id + p.relation)}
                              {@const isMcp = p.source_type === "mcp_client" || p.source_type === "proposal"}
                              <li class={isMcp ? "is-mcp" : ""}>
                                {#if isMcp}<span class="mcp-tag">MCP</span>{/if}
                                <span class="rel">{p.relation}</span>
                                <span class="src-type">({p.source_type})</span>
                                {#if p.source_type === "turn" && p.conversation_id && onOpenSource}
                                  <button
                                    type="button"
                                    class="preview src-link"
                                    title="Open the source message in chat"
                                    onclick={() => onOpenSource!(p.conversation_id!, p.source_id)}
                                  >{p.preview ?? "view source"} ↗</button>
                                {:else if p.preview}
                                  <span class="preview">{p.preview}</span>
                                {/if}
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
      {/snippet}

      <!-- ===================== LEDGER: topic cards or flat list ===================== -->
      {#if groupView === "topics"}
        <div class="topics">
          {#each topicGroups as g (g.key)}
            {@const open = expandedTopics.has(g.key)}
            {@const rev = reviewCount(g.items)}
            <div class="topic-card" class:open>
              <button class="topic-head" onclick={() => toggleTopic(g.key)}>
                <span class="topic-chevron">{open ? "▾" : "▸"}</span>
                <span class="topic-name">{g.label}</span>
                <span class="topic-count num">{g.items.length}</span>
                {#if rev > 0}
                  <span class="topic-review">{rev} to review</span>
                {/if}
                {#if !open && g.items[0]}
                  <span class="topic-preview">{g.items[0].statement}</span>
                {/if}
              </button>
              {#if open}
                <ol class="ledger">
                  {#each g.items as b, i (b.id)}
                    {@render beliefRow(b, i)}
                  {/each}
                </ol>
              {/if}
            </div>
          {/each}
        </div>
      {:else}
        <div class="ledger-head">
          <span>№</span>
          <span>Belief</span>
          <span style="text-align: right;" title={CONF_TITLE}>Consistency</span>
          <span style="text-align: right;">Cites</span>
          <span style="text-align: right;">Updated</span>
        </div>
        <ol class="ledger">
          {#each visible as b, idx (b.id)}
            {@render beliefRow(b, idx)}
          {/each}
        </ol>
      {/if}
    {/if}
  </div>
</aside>

<style>
  /* ============================================================
     Memory Ledger — columnar Index style
     ============================================================ */

  .audit-aside {
    height: 100%;
    box-shadow: var(--pal-shadow-lg);
  }

  /* small icon buttons (refresh, close) */
  .audit-icon {
    background: transparent;
    color: var(--pal-dim);
    border: 1px solid transparent;
    border-radius: var(--pal-radius);
    padding: 2px 6px;
    font-size: 13px;
    line-height: 1;
    cursor: pointer;
  }
  .audit-icon:hover { color: var(--pal-ink); border-color: var(--pal-border); }
  .audit-icon:disabled { opacity: 0.4; cursor: default; }

  /* Tools button (with badge) */
  .audit-tools-btn {
    position: relative;
    background: transparent;
    color: var(--pal-ink);
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 3px 10px;
    font-size: 11px;
    font-weight: 500;
    letter-spacing: 0.04em;
    cursor: pointer;
  }
  .audit-tools-btn:hover { border-color: rgb(var(--pal-accent)); color: rgb(var(--pal-accent)); }
  .tools-badge {
    position: absolute;
    top: -5px;
    right: -5px;
    background: rgb(var(--pal-accent));
    color: white;
    font-size: 9px;
    line-height: 1;
    padding: 2px 4px;
    border-radius: 6px;
    min-width: 12px;
    text-align: center;
  }

  /* Filter chips */
  .filter-chip {
    background: transparent;
    color: var(--pal-dim);
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 2px 8px;
    font-size: 11px;
    line-height: 1.5;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-family: "IBM Plex Sans";
  }
  .filter-chip .glyph { font-size: 10px; line-height: 1; }
  .filter-chip:hover { color: var(--pal-ink); border-color: var(--pal-ink); }
  .filter-chip.active { background: var(--pal-accent-soft); border-color: rgb(var(--pal-accent)); color: rgb(var(--pal-accent)); }
  .filter-chip:disabled { opacity: 0.5; cursor: default; }

  /* Hairline separator between status chips and trust chips in the unified row. */
  .chip-divider {
    display: inline-block;
    width: 1px;
    height: 14px;
    background: var(--pal-border);
    margin: 0 4px;
  }

  /* Ledger column header */
  .ledger-head {
    display: grid;
    grid-template-columns: 28px 1fr 96px 32px 44px;
    gap: 10px;
    padding: 8px 16px 6px;
    font-size: 9.5px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--pal-dim);
    border-bottom: 1px solid var(--pal-border);
    font-weight: 500;
  }

  /* Ledger list */
  .ledger { margin: 0; padding: 0; list-style: none; }

  .ledger-row { position: relative; }
  .ledger-row.warn::before {
    content: "";
    position: absolute; left: 0; top: 0; bottom: 0; width: 2px;
    background: rgb(var(--pal-warn));
  }
  .ledger-row.summary::before {
    content: "";
    position: absolute; left: 0; top: 0; bottom: 0; width: 2px;
    background: rgb(var(--pal-accent));
  }
  .ledger-row.expanded { background: rgba(0,0,0,0.10); }
  :global(.dark) .ledger-row.expanded { background: rgba(255,255,255,0.02); }

  .ledger-row-button {
    width: 100%;
    display: grid;
    grid-template-columns: 28px 1fr 96px 32px 44px;
    gap: 10px;
    align-items: center;
    padding: 8px 16px;
    background: transparent;
    border: none;
    border-bottom: 1px dotted var(--pal-border);
    text-align: left;
    cursor: pointer;
    color: var(--pal-ink);
    font-family: "IBM Plex Sans";
    font-size: 12.5px;
  }
  .ledger-row-button:hover { background: rgba(0,0,0,0.05); }
  :global(.dark) .ledger-row-button:hover { background: rgba(255,255,255,0.025); }

  .col-n { font-size: 10.5px; color: var(--pal-dim); text-align: right; }

  .col-stmt { display: flex; align-items: baseline; gap: 8px; min-width: 0; }
  .trust-glyph { color: var(--pal-ink); font-size: 11px; line-height: 1; flex-shrink: 0; }
  .stmt-text {
    color: var(--pal-ink);
    line-height: 1.4;
    overflow: hidden;
    text-overflow: ellipsis;
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
  }
  .ledger-row.warn .stmt-text { color: rgb(var(--pal-warn)); }
  .ledger-row.dim .stmt-text { color: var(--pal-dim); text-decoration: line-through; }
  .ledger-row.dim .trust-glyph { color: var(--pal-dim); }

  /* Confidence column — coarse buckets, never a false-precise number */
  .col-conf { display: flex; align-items: center; gap: 6px; justify-content: flex-end; }
  .conf-chip {
    font-size: 9.5px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    padding: 1px 6px;
    border-radius: 999px;
    border: 1px solid var(--pal-border);
    color: var(--pal-dim);
    white-space: nowrap;
  }
  .conf-chip.conf-strong {
    color: rgb(var(--pal-accent));
    border-color: rgba(var(--pal-accent), 0.5);
  }
  .conf-chip.conf-moderate { color: var(--pal-ink); }
  .conf-chip.conf-tentative { color: var(--pal-dim); opacity: 0.85; }
  /* Clickable source-utterance link in provenance */
  .src-link {
    background: none;
    border: none;
    padding: 0;
    cursor: pointer;
    text-align: left;
    color: var(--pal-dim);
    text-decoration: underline dotted;
  }
  .src-link:hover { color: rgb(var(--pal-accent)); }

  /* Anchors — persistent reference strip of what the user has stated/confirmed */
  .anchors {
    padding: 10px 16px;
    border-bottom: 1px solid var(--pal-border);
    background: var(--pal-accent-soft);
  }
  .anchors-head { margin-bottom: 6px; color: var(--pal-dim); }
  .anchors ul { display: flex; flex-direction: column; gap: 2px; }
  .anchor-row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 3px 4px;
    border-radius: 4px;
    text-align: left;
    cursor: pointer;
    background: none;
    border: none;
    color: var(--pal-ink);
  }
  .anchor-row:hover { background: var(--pal-bg-sunken); }
  .anchor-stmt {
    flex: 1;
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .col-cites { font-size: 10.5px; color: var(--pal-dim); text-align: right; }
  .col-time { font-size: 10.5px; color: var(--pal-dim); text-align: right; }

  /* Expand */
  .expand {
    padding: 10px 16px 14px 28px;
    border-bottom: 1px solid var(--pal-border);
    font-size: 12px;
  }

  .children {
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 8px 10px;
    margin-bottom: 10px;
    background: var(--pal-surface);
  }
  .children ul { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  .children li { display: flex; align-items: baseline; gap: 8px; font-size: 11.5px; }
  .children .num { font-size: 10px; }

  .actions {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-wrap: wrap;
    margin-bottom: 10px;
  }
  .row-action {
    background: transparent;
    border: 1px solid var(--pal-border);
    color: var(--pal-dim);
    border-radius: var(--pal-radius);
    padding: 3px 9px;
    font-size: 11px;
    font-family: "IBM Plex Sans";
    cursor: pointer;
    line-height: 1.4;
  }
  .row-action:hover { color: var(--pal-ink); border-color: var(--pal-ink); }
  .row-action:disabled { opacity: 0.4; cursor: default; }
  .row-action.accent {
    background: rgb(var(--pal-accent));
    color: white;
    border-color: rgb(var(--pal-accent));
  }
  .row-action.accent:hover { opacity: 0.9; }
  .row-action.accept { color: var(--pal-ink); border-color: var(--pal-border); }
  .row-action.accept:hover { border-color: rgb(var(--pal-accent)); color: rgb(var(--pal-accent)); }
  .row-action.warn-action { color: rgb(var(--pal-warn)); border-color: rgba(184,146,74,0.4); }
  .row-action.warn-action:hover { background: var(--pal-warn-soft); }

  .action-form {
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 10px;
    margin-bottom: 10px;
    background: var(--pal-surface);
    display: flex; flex-direction: column; gap: 8px;
  }
  .action-form label { display: flex; flex-direction: column; gap: 4px; }
  .action-input {
    background: transparent;
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 4px 8px;
    color: var(--pal-ink);
    font-size: 12px;
    font-family: "IBM Plex Sans";
    width: 100%;
  }
  .action-input:focus { border-color: rgb(var(--pal-accent)); outline: none; }
  .action-check { flex-direction: row !important; align-items: center; gap: 8px; color: var(--pal-dim); text-transform: none; letter-spacing: 0; font-size: 11px; }
  .action-form-foot { display: flex; justify-content: flex-end; gap: 6px; margin-top: 2px; }

  .versions { display: flex; flex-direction: column; gap: 8px; }
  .version {
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    padding: 8px 10px;
    background: var(--pal-surface);
  }
  .version-head { display: flex; justify-content: space-between; align-items: baseline; margin-bottom: 4px; }
  .version-stmt { margin: 0; color: var(--pal-ink); font-size: 12px; line-height: 1.45; }
  .version-reason { margin: 4px 0 0; color: var(--pal-dim); font-style: italic; font-size: 11px; }
  .provenance { list-style: none; margin: 6px 0 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
  .provenance li {
    font-size: 10.5px;
    color: var(--pal-dim);
    padding-left: 8px;
    border-left: 2px solid var(--pal-border);
    line-height: 1.5;
  }
  .provenance li.is-mcp { border-left-color: rgb(var(--pal-warn)); color: rgb(var(--pal-warn)); }
  .provenance .mcp-tag {
    font-family: "IBM Plex Mono";
    font-size: 9px;
    letter-spacing: 0.1em;
    color: rgb(var(--pal-warn));
    padding-right: 4px;
  }
  .provenance .rel { font-weight: 500; }
  .provenance .src-type, .provenance .preview { color: var(--pal-dim); }
  .provenance .preview { font-style: italic; }

  /* Segmented Topics/List toggle */
  .seg {
    display: inline-flex;
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius-sm);
    overflow: hidden;
  }
  .seg-btn {
    background: transparent;
    border: none;
    color: var(--pal-dim);
    font-size: 10.5px;
    padding: 2px 9px;
    cursor: pointer;
    font-family: "IBM Plex Sans";
  }
  .seg-btn:hover { color: var(--pal-ink); }
  .seg-btn.active { background: var(--pal-accent-soft); color: rgb(var(--pal-accent)); }

  /* Topic cards — collapsed groups that decrowd the ledger. */
  .topics { padding: 10px 12px; display: flex; flex-direction: column; gap: 8px; }
  .topic-card {
    border: 1px solid var(--pal-border);
    border-radius: var(--pal-radius);
    background: var(--pal-surface);
    box-shadow: var(--pal-shadow);
    overflow: hidden;
    transition: box-shadow 140ms ease;
  }
  .topic-card.open { box-shadow: var(--pal-shadow-lg); }
  .topic-head {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 9px 12px;
    background: transparent;
    border: none;
    cursor: pointer;
    text-align: left;
    color: var(--pal-ink);
    font-family: "IBM Plex Sans";
  }
  .topic-head:hover { background: rgba(127, 127, 127, 0.06); }
  .topic-chevron { color: var(--pal-dim); font-size: 10px; width: 10px; flex-shrink: 0; }
  .topic-name { font-size: 12.5px; font-weight: 500; flex-shrink: 0; }
  .topic-count {
    font-size: 10px;
    color: var(--pal-dim);
    border: 1px solid var(--pal-border);
    border-radius: 999px;
    padding: 0 6px;
    flex-shrink: 0;
  }
  .topic-review {
    font-size: 9.5px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: rgb(var(--pal-warn));
    flex-shrink: 0;
  }
  .topic-preview {
    flex: 1;
    min-width: 0;
    font-size: 11px;
    color: var(--pal-dim);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-align: right;
  }
  .topic-card .ledger { border-top: 1px solid var(--pal-border); }

  /* Recently-merged digest rows */
  .merge-rec { line-height: 1.4; }
</style>
