<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { UMAP } from "umap-js";
  import {
    getGraphSnapshot,
    getGraphEdgesExtended,
    getTurnsForBelief,
    getBeliefEmbeddings,
    saveBeliefPositions,
    getBeliefDetail,
    regenerateBeliefLabels,
    nameCluster,
    type GraphBelief,
    type GraphEdge,
    type GraphEdgeKind,
    type BeliefDetail,
    type TurnForBelief,
  } from "./chat";
  import { themeState } from "./theme.svelte";

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  // ============================================================
  // State
  // ============================================================

  let beliefs: GraphBelief[] = $state([]);
  let edges: GraphEdge[] = $state([]);
  let extEdges: GraphEdge[] = $state([]);
  let extEdgesLoaded = $state(false);
  let projectionVersion = $state(0);
  let loading = $state(true);
  let projecting = $state(false);
  let status: string = $state("");

  // Trust-class filters (Phase A)
  let filterAsserted = $state(true);
  let filterInferred = $state(true);
  let filterHypothesized = $state(true);
  let filterSummary = $state(true);

  // Status visibility (Phase B)
  let showCorrected = $state(true);
  let showContested = $state(true);
  let showExpired = $state(false);
  let showBlocked = $state(false);
  let statusOpen = $state(false);

  // Edge-type filters (Phase A)
  let edgeFilters: Record<GraphEdgeKind, boolean> = $state({
    hierarchy: true,
    summarizes: true,
    reinforced_by: true,
    contradicted_by: true,
    corrected_by: true,
    extracted_from: false,
    knn: false,
    co_recall: false,
  });

  // Search (Phase C)
  let searchQuery = $state("");

  // Focus mode (Phase C)
  let focusedId: string | null = $state(null);

  // Receipts spotlight (Phase C)
  let turnsForSelected: TurnForBelief[] = $state([]);
  let spotlightTurnId: string | null = $state(null);
  let spotlightBeliefIds: Set<string> = $state(new Set());

  // Edge hover inspector (Phase C)
  let hoveredEdge: GraphEdge | null = $state(null);

  // Legend (Phase B)
  let legendOpen = $state(false);

  // Advanced controls menu — everything that isn't trust-class chips / search
  // / play / reset / close lives behind this single overflow popover.
  let advancedOpen = $state(false);

  // Label backfill (post-Phase D feedback)
  let labeling = $state(false);
  let labelStatus = $state<string | null>(null);

  // Time scrubber (Phase D)
  let timeMode = $state(false);
  let scrubberMs: number | null = $state(null); // null → now
  let replayPlaying = $state(false);
  const REPLAY_DAYS_PER_SEC = 3.0;
  let replayLastFrame = 0;

  // Human-friendly date formatters for the time controls. Avoids ISO
  // strings ("2026-05-28T…") in the chrome.
  const MONTHS = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];

  function fmtShortDate(ms: number): string {
    const d = new Date(ms);
    const sameYear = d.getFullYear() === new Date().getFullYear();
    const month = MONTHS[d.getMonth()];
    return sameYear
      ? `${month} ${d.getDate()}`
      : `${month} ${d.getDate()}, ${d.getFullYear()}`;
  }

  /** "now", "12m ago", "3h ago", "5d ago", "2w ago", "4mo ago", "1y ago". */
  function fmtRelative(ms: number): string {
    const diff = Date.now() - ms;
    if (diff < 60_000) return "now";
    const m = Math.floor(diff / 60_000);
    if (m < 60) return `${m}m ago`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}h ago`;
    const d = Math.floor(h / 24);
    if (d < 14) return `${d}d ago`;
    const w = Math.floor(d / 7);
    if (w < 9) return `${w}w ago`;
    const mo = Math.floor(d / 30);
    if (mo < 18) return `${mo}mo ago`;
    return `${Math.floor(d / 365)}y ago`;
  }

  // Stale corner (Phase D)
  let staleCornerOn = $state(false);

  let canvasEl: HTMLCanvasElement;
  let dpr = 1;
  let viewport = $state({ scale: 1, offsetX: 0, offsetY: 0 });
  let hoverId: string | null = $state(null);
  let selected: BeliefDetail | null = $state(null);

  type ScreenPoint = { id: string; sx: number; sy: number; r: number };
  type ScreenEdge = {
    edge: GraphEdge;
    sx: number; sy: number; tx: number; ty: number;
  };
  let screenPoints: ScreenPoint[] = [];
  let screenEdges: ScreenEdge[] = [];

  // ============================================================
  // Palette
  // ============================================================

  type Palette = {
    bgGradTop: string; bgGradBot: string;
    ink: string; inkSoft: string; inkFaint: string;
    gridMinor: string; gridMajor: string; ticks: string;
    regionRing: string; connection: string; crosshair: string;
    asserted: string; inferred: string; hypothesized: string;
    summary: string; summaryCore: string;
    accent: string;
    edgeHierarchy: string; edgeSummarizes: string;
    edgeReinforced: string; edgeContradicted: string;
    edgeCorrected: string; edgeKnn: string; edgeCoRecall: string;
    danger: string;
  };

  // Civic Archive — graphite + vermilion chop seal + ochre for contradictions.
  // No gradients (flat dark surface). Edges that contradict get ochre, not red.
  const DARK: Palette = {
    bgGradTop: "#1E1F23", bgGradBot: "#1E1F23",     // flat graphite
    ink: "#E8E2D6",                                   // warm bone
    inkSoft: "rgba(232,226,214,0.62)",
    inkFaint: "rgba(232,226,214,0.35)",
    gridMinor: "rgba(232,226,214,0.05)",              // drafting paper, very faint
    gridMajor: "rgba(232,226,214,0.10)",
    ticks: "rgba(232,226,214,0.40)",
    regionRing: "rgba(232,226,214,0.12)",             // cluster contour ring base
    connection: "rgba(232,226,214,0.04)",
    crosshair: "rgba(209,79,63,0.18)",                // vermilion
    asserted: "#E8E2D6",                              // solid warm bone
    inferred: "rgba(232,226,214,0.55)",               // half-fill ghost
    hypothesized: "#88847b",                          // dim ring only
    summary: "#D14F3F",                               // vermilion chop seal
    summaryCore: "#1E1F23",                           // canvas color for inner cut-out
    accent: "#D14F3F",                                // vermilion
    edgeHierarchy: "rgba(232,226,214,0.30)",
    edgeSummarizes: "rgba(209,79,63,0.55)",           // vermilion ridge
    edgeReinforced: "rgba(232,226,214,0.45)",
    edgeContradicted: "rgba(184,146,74,0.85)",        // ochre fault line (the warn token)
    edgeCorrected: "rgba(184,146,74,0.65)",
    edgeKnn: "rgba(232,226,214,0.08)",
    edgeCoRecall: "rgba(209,79,63,0.12)",
    danger: "#D14F3F",
  };

  const LIGHT: Palette = {
    bgGradTop: "#F7F3EC", bgGradBot: "#F7F3EC",      // warm paper, flat
    ink: "#1B1B1F",                                    // deep ink
    inkSoft: "rgba(27,27,31,0.62)",
    inkFaint: "rgba(27,27,31,0.40)",
    gridMinor: "rgba(27,27,31,0.05)",
    gridMajor: "rgba(27,27,31,0.10)",
    ticks: "rgba(27,27,31,0.45)",
    regionRing: "rgba(27,27,31,0.12)",
    connection: "rgba(27,27,31,0.06)",
    crosshair: "rgba(209,79,63,0.30)",
    asserted: "#1B1B1F",
    inferred: "rgba(27,27,31,0.55)",
    hypothesized: "#6e6a60",
    summary: "#D14F3F",
    summaryCore: "#F7F3EC",
    accent: "#D14F3F",
    edgeHierarchy: "rgba(27,27,31,0.45)",
    edgeSummarizes: "rgba(209,79,63,0.60)",
    edgeReinforced: "rgba(27,27,31,0.50)",
    edgeContradicted: "rgba(184,146,74,0.85)",
    edgeCorrected: "rgba(184,146,74,0.65)",
    edgeKnn: "rgba(27,27,31,0.10)",
    edgeCoRecall: "rgba(209,79,63,0.15)",
    danger: "#D14F3F",
  };

  let palette = $derived(themeState.current === "light" ? LIGHT : DARK);

  // Category hues — orthogonal channel from trust class.
  const CATEGORY_HUE: Record<string, number> = {
    preference: 30,
    fact: 200,
    skill: 145,
    plan: 280,
    context: 0,
    other: 60,
  };

  // Civic Archive: nodes are monochrome — bone for asserted, half-fill for inferred,
  // hairline ring for hypothesized, vermilion for summaries. No category hue tinting.
  // (CATEGORY_HUE kept above for the legend's optional category panel, but tint
  // is suppressed on the canvas.)
  function categoryTint(_cat: string | null): string | null {
    return null;
  }

  // ============================================================
  // Clustering — k-means on 2D positions, auto-K via silhouette.
  // Drives the Survey-map contour regions. Cheap (n ≤ a few hundred);
  // re-runs whenever the projection version changes.
  // ============================================================

  type Cluster = {
    id: number;
    cx: number; cy: number;     // centroid (world coords)
    sx: number; sy: number;     // std-dev along each axis (world coords)
    meanConfidence: number;
    members: GraphBelief[];
  };
  let clusters: Cluster[] = $state([]);
  /** projection_version this cluster set was computed for. */
  let clustersForVersion = $state<number | null>(null);
  /** LLM-generated cluster names, keyed by cluster id (resets each re-cluster). */
  let clusterLabels = $state<Map<number, string>>(new Map());
  let clusterLabelingVersion = -1;

  function _distSq(ax: number, ay: number, bx: number, by: number): number {
    const dx = ax - bx, dy = ay - by;
    return dx * dx + dy * dy;
  }

  /** k-means++ init + Lloyd iteration. Deterministic-ish (seeded by point order). */
  function kmeans2D(pts: { x: number; y: number }[], k: number, iters = 30): number[] {
    const n = pts.length;
    if (n === 0 || k <= 0) return [];
    if (k === 1) return new Array(n).fill(0);
    const cents: [number, number][] = [];
    // k-means++ seeding
    cents.push([pts[0].x, pts[0].y]);
    for (let c = 1; c < k; c++) {
      const dists = pts.map((p) => {
        let m = Infinity;
        for (const ct of cents) {
          const d = _distSq(p.x, p.y, ct[0], ct[1]);
          if (d < m) m = d;
        }
        return m;
      });
      const total = dists.reduce((a, b) => a + b, 0);
      let r = Math.random() * total;
      let picked = pts.length - 1;
      for (let i = 0; i < n; i++) {
        r -= dists[i];
        if (r <= 0) { picked = i; break; }
      }
      cents.push([pts[picked].x, pts[picked].y]);
    }
    const assign = new Array<number>(n).fill(0);
    for (let it = 0; it < iters; it++) {
      let changed = false;
      for (let i = 0; i < n; i++) {
        let best = 0, bestD = Infinity;
        for (let c = 0; c < k; c++) {
          const d = _distSq(pts[i].x, pts[i].y, cents[c][0], cents[c][1]);
          if (d < bestD) { bestD = d; best = c; }
        }
        if (assign[i] !== best) { assign[i] = best; changed = true; }
      }
      if (!changed) break;
      const sums: [number, number, number][] = Array.from({ length: k }, () => [0, 0, 0]);
      for (let i = 0; i < n; i++) {
        const c = assign[i];
        sums[c][0] += pts[i].x;
        sums[c][1] += pts[i].y;
        sums[c][2] += 1;
      }
      for (let c = 0; c < k; c++) {
        if (sums[c][2] > 0) cents[c] = [sums[c][0] / sums[c][2], sums[c][1] / sums[c][2]];
      }
    }
    return assign;
  }

  /** Silhouette score — mean over points; range [-1, 1], higher is better. */
  function silhouette(pts: { x: number; y: number }[], assign: number[], k: number): number {
    const n = pts.length;
    if (k <= 1 || k >= n) return -1;
    const groups: number[][] = Array.from({ length: k }, () => []);
    assign.forEach((c, i) => groups[c].push(i));
    let total = 0, count = 0;
    for (let i = 0; i < n; i++) {
      const own = assign[i];
      if (groups[own].length <= 1) continue;
      let a = 0;
      for (const j of groups[own]) if (j !== i)
        a += Math.sqrt(_distSq(pts[i].x, pts[i].y, pts[j].x, pts[j].y));
      a /= (groups[own].length - 1);
      let b = Infinity;
      for (let c = 0; c < k; c++) {
        if (c === own || groups[c].length === 0) continue;
        let m = 0;
        for (const j of groups[c])
          m += Math.sqrt(_distSq(pts[i].x, pts[i].y, pts[j].x, pts[j].y));
        m /= groups[c].length;
        if (m < b) b = m;
      }
      if (!Number.isFinite(b)) continue;
      const s = (b - a) / Math.max(a, b);
      total += s;
      count += 1;
    }
    return count > 0 ? total / count : 0;
  }

  function recomputeClusters() {
    // Skip if we've already clustered this projection version.
    if (clustersForVersion === projectionVersion && clusters.length > 0) return;
    // Only beliefs with a 2D position and an embedding participate.
    // Summaries are excluded — they're meta-beliefs, not topical members.
    const candidates = beliefs.filter(
      (b) => b.x !== null && b.y !== null && b.has_embedding && b.trust_class !== "summary",
    );
    if (candidates.length < 4) {
      clusters = [];
      clustersForVersion = projectionVersion;
      return;
    }
    const pts = candidates.map((b) => ({ x: b.x as number, y: b.y as number }));
    // Auto-K: try 2..min(7, sqrt(n/2)); pick best silhouette.
    const maxK = Math.max(2, Math.min(7, Math.floor(Math.sqrt(candidates.length / 2))));
    let bestK = 2, bestScore = -Infinity, bestAssign: number[] = [];
    for (let k = 2; k <= maxK; k++) {
      const assign = kmeans2D(pts, k);
      const score = silhouette(pts, assign, k);
      if (score > bestScore) { bestScore = score; bestK = k; bestAssign = assign; }
    }
    const groups: Cluster[] = Array.from({ length: bestK }, (_, i) => ({
      id: i, cx: 0, cy: 0, sx: 0, sy: 0, members: [], meanConfidence: 0,
    }));
    for (let i = 0; i < candidates.length; i++) groups[bestAssign[i]].members.push(candidates[i]);
    for (const c of groups) {
      if (c.members.length === 0) continue;
      c.cx = c.members.reduce((a, m) => a + (m.x as number), 0) / c.members.length;
      c.cy = c.members.reduce((a, m) => a + (m.y as number), 0) / c.members.length;
      c.sx = Math.sqrt(
        c.members.reduce((a, m) => a + ((m.x as number) - c.cx) ** 2, 0) / c.members.length,
      ) || 0.05;
      c.sy = Math.sqrt(
        c.members.reduce((a, m) => a + ((m.y as number) - c.cy) ** 2, 0) / c.members.length,
      ) || 0.05;
      c.meanConfidence =
        c.members.reduce((a, m) => a + m.confidence, 0) / c.members.length;
    }
    clusters = groups.filter((c) => c.members.length >= 2);
    clustersForVersion = projectionVersion;
    // Fire-and-forget LLM cluster naming, cached per projection_version.
    nameClustersAsync();
  }

  async function nameClustersAsync() {
    if (clusterLabelingVersion === projectionVersion) return;
    clusterLabelingVersion = projectionVersion;
    const snapshot = clusters.map((c) => ({ id: c.id, statements: c.members.slice(0, 6).map((m) => m.statement) }));
    for (const c of snapshot) {
      try {
        const label = await nameCluster(c.statements);
        // Only set if this cluster set is still current (no later projection swooped in).
        if (clusterLabelingVersion === projectionVersion) {
          clusterLabels = new Map(clusterLabels).set(c.id, label);
        }
      } catch {
        // Network/LLM hiccup — leave unlabeled; legend falls back to "Region N".
      }
    }
  }

  function withAlpha(hex: string, a: number): string {
    if (hex.startsWith("rgba")) return hex.replace(/[\d.]+\)$/, a + ")");
    if (hex.startsWith("rgb(")) return hex.replace("rgb(", "rgba(").replace(")", "," + a + ")");
    if (hex.startsWith("hsl(")) return hex.replace("hsl(", "hsla(").replace(")", `, ${a})`);
    if (hex.startsWith("hsla")) return hex.replace(/[\d.]+\)$/, a + ")");
    const h = hex.replace("#", "");
    const r = parseInt(h.substring(0, 2), 16);
    const g = parseInt(h.substring(2, 4), 16);
    const b = parseInt(h.substring(4, 6), 16);
    return `rgba(${r},${g},${b},${a})`;
  }

  // ============================================================
  // Recency / staleness
  // ============================================================

  function ageDays(b: GraphBelief): number {
    const ref = b.last_reinforced_at ?? b.created_at;
    return (effectiveNowMs() - new Date(ref).getTime()) / 86_400_000;
  }
  function recencyAlpha(d: number): number {
    if (d <= 7) return 1.0;
    if (d >= 365) return 0.40;
    return 1.0 - (d / 365) * 0.60;
  }
  function recencyGlow(d: number): number {
    if (d <= 1) return 1.6;
    if (d <= 7) return 1.25;
    return 1.0;
  }
  function recencyLabel(d: number): string {
    if (d < 1) return "today";
    if (d < 30) return `${Math.round(d)}d ago`;
    if (d < 365) return `${Math.round(d / 30)}mo ago`;
    return `${Math.round(d / 365)}y ago`;
  }

  function idPhase(id: string): number {
    let h = 0;
    for (let i = 0; i < id.length; i++) h = (h * 31 + id.charCodeAt(i)) | 0;
    return ((h % 1000) / 1000) * Math.PI * 2;
  }

  // ============================================================
  // Filtering
  // ============================================================

  function trustFiltered(b: GraphBelief): boolean {
    switch (b.trust_class) {
      case "asserted": return filterAsserted;
      case "inferred": return filterInferred;
      case "hypothesized": return filterHypothesized;
      case "summary": return filterSummary;
    }
  }

  function statusFiltered(b: GraphBelief): boolean {
    if (b.status === "corrected") return showCorrected;
    if (b.status === "contested") return showContested;
    if (b.status === "expired") return showExpired;
    if (b.status === "blocked") return showBlocked;
    return true;
  }

  function effectiveNowMs(): number {
    return scrubberMs ?? Date.now();
  }

  function timeFiltered(b: GraphBelief): boolean {
    if (scrubberMs === null) return true;
    return new Date(b.created_at).getTime() <= scrubberMs;
  }

  function searchMatch(b: GraphBelief): boolean {
    if (!searchQuery.trim()) return true;
    return b.statement.toLowerCase().includes(searchQuery.trim().toLowerCase());
  }

  // ============================================================
  // Load + project
  // ============================================================

  async function load() {
    loading = true;
    status = "loading…";
    const snap = await getGraphSnapshot();
    beliefs = snap.beliefs;
    edges = snap.edges;
    projectionVersion = snap.projection_version;
    loading = false;

    const projectable = beliefs.filter((b) => b.has_embedding);
    const missing = projectable.filter((b) => b.x === null);
    if (missing.length > 0 && projectable.length >= 8) {
      await runProjection(projectable.map((b) => b.id));
    } else if (projectable.length < 8) {
      status = `Need ≥ 8 embedded beliefs (have ${projectable.length}).`;
    } else {
      status = `${beliefs.length} beliefs · ${edges.length} edges · v${projectionVersion}`;
    }
    recomputeClusters();
    fitAndRender();
  }

  async function runProjection(belief_ids: string[]) {
    projecting = true;
    status = `embedding ${belief_ids.length}…`;
    try {
      const rows = await getBeliefEmbeddings(belief_ids);
      if (rows.length < 8) {
        status = `Got ${rows.length} embeddings; need ≥ 8.`;
        return;
      }
      const ids = rows.map((r) => r[0]);
      const data = rows.map((r) => r[1]);
      status = `running UMAP on ${rows.length} × ${data[0].length}-D…`;
      const nNeighbors = Math.min(15, rows.length - 1);
      const umap = new UMAP({ nComponents: 2, nNeighbors, minDist: 0.1, spread: 1.0 });
      const projected = umap.fit(data);

      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
      for (const [x, y] of projected) {
        if (x < minX) minX = x;
        if (y < minY) minY = y;
        if (x > maxX) maxX = x;
        if (y > maxY) maxY = y;
      }
      const cx = (minX + maxX) / 2;
      const cy = (minY + maxY) / 2;
      const span = Math.max(maxX - minX, maxY - minY) / 2 || 1;
      const positions = projected.map(([x, y], i) => ({
        belief_id: ids[i],
        x: (x - cx) / span,
        y: (y - cy) / span,
      }));

      const newVersion = await saveBeliefPositions(positions);
      projectionVersion = newVersion;
      const snap = await getGraphSnapshot();
      beliefs = snap.beliefs;
      edges = snap.edges;
      status = `${beliefs.length} beliefs · ${edges.length} edges · v${newVersion}`;
      recomputeClusters();
    } catch (e) {
      status = `projection failed: ${e}`;
    } finally {
      projecting = false;
    }
  }

  async function backfillLabels() {
    if (labeling) return;
    labeling = true;
    labelStatus = "labeling…";
    try {
      const r = await regenerateBeliefLabels();
      const errSuffix = r.first_error ? ` — ${r.first_error}` : "";
      labelStatus = `+${r.labeled} labeled${r.failed ? ` · ${r.failed} failed${errSuffix}` : ""}`;
      // Reload snapshot so the new labels show.
      const snap = await getGraphSnapshot();
      beliefs = snap.beliefs;
      edges = snap.edges;
    } catch (e) {
      labelStatus = `labeling failed: ${e}`;
    } finally {
      labeling = false;
      setTimeout(() => (labelStatus = null), 4000);
    }
  }

  async function loadExtendedEdges() {
    if (extEdgesLoaded) return;
    try {
      const ext = await getGraphEdgesExtended(3);
      extEdges = ext;
      extEdgesLoaded = true;
    } catch (e) {
      console.warn("extended edges failed", e);
    }
  }

  // ============================================================
  // Derived: visible beliefs, adjacency, focus reachability, search
  // ============================================================

  let visibleBeliefs = $derived(
    beliefs.filter(
      (b) =>
        b.x !== null &&
        b.y !== null &&
        trustFiltered(b) &&
        statusFiltered(b) &&
        timeFiltered(b),
    ),
  );

  let visibleIdSet = $derived(new Set(visibleBeliefs.map((b) => b.id)));

  // Combined edges (snapshot + optional extended), filtered by toggle and visibility.
  let activeEdges = $derived.by((): GraphEdge[] => {
    const all = [
      ...edges,
      ...(extEdgesLoaded
        ? extEdges.filter((e) => edgeFilters[e.kind])
        : []),
    ];
    return all.filter(
      (e) =>
        edgeFilters[e.kind] &&
        visibleIdSet.has(e.source_id) &&
        visibleIdSet.has(e.target_id),
    );
  });

  // Adjacency map for ego graph + 1-hop search dim.
  let adjacency = $derived.by((): Map<string, Set<string>> => {
    const adj = new Map<string, Set<string>>();
    for (const e of activeEdges) {
      if (!adj.has(e.source_id)) adj.set(e.source_id, new Set());
      if (!adj.has(e.target_id)) adj.set(e.target_id, new Set());
      adj.get(e.source_id)!.add(e.target_id);
      adj.get(e.target_id)!.add(e.source_id);
    }
    return adj;
  });

  // Ego graph at 2 hops from focusedId.
  let focusReach = $derived.by((): Set<string> | null => {
    if (!focusedId) return null;
    const r = new Set<string>([focusedId]);
    let frontier = new Set<string>([focusedId]);
    for (let hop = 0; hop < 2; hop++) {
      const next = new Set<string>();
      for (const id of frontier) {
        const ns = adjacency.get(id);
        if (!ns) continue;
        for (const n of ns) {
          if (!r.has(n)) {
            r.add(n);
            next.add(n);
          }
        }
      }
      frontier = next;
    }
    return r;
  });

  // Search: ids matching the query + their 1-hop neighbors get medium dim.
  let searchMatchSet = $derived.by((): Set<string> | null => {
    const q = searchQuery.trim().toLowerCase();
    if (!q) return null;
    return new Set(visibleBeliefs.filter((b) => b.statement.toLowerCase().includes(q)).map((b) => b.id));
  });
  let searchHaloSet = $derived.by((): Set<string> => {
    if (!searchMatchSet) return new Set();
    const halo = new Set<string>();
    for (const id of searchMatchSet) {
      halo.add(id);
      const ns = adjacency.get(id);
      if (ns) for (const n of ns) halo.add(n);
    }
    return halo;
  });

  // Summary→children index (Phase B convex hulls).
  let summaryChildren = $derived.by((): Map<string, GraphBelief[]> => {
    const m = new Map<string, GraphBelief[]>();
    for (const b of visibleBeliefs) {
      if (b.parent_summary_id) {
        const parent = b.parent_summary_id;
        if (!m.has(parent)) m.set(parent, []);
        m.get(parent)!.push(b);
      }
    }
    return m;
  });

  // ============================================================
  // Projection: world → screen
  // ============================================================

  function baseScale(w: number, h: number): number {
    const padding = 80 * dpr;
    return (Math.min(w, h) - padding * 2) / 2;
  }

  function isStale(b: GraphBelief): boolean {
    if (!staleCornerOn) return false;
    return ageDays(b) > 90;
  }

  function project(
    wx: number, wy: number, id: string, t: number, w: number, h: number,
    stale = false,
  ): [number, number] {
    const phase = idPhase(id);
    const speed = 1.6;
    const dx = Math.sin(t * 0.18 * speed + phase) * 3 * dpr;
    const dy = Math.cos(t * 0.14 * speed + phase * 1.3) * 3 * dpr;

    if (stale) {
      const cornerX = w * 0.10 + (Math.sin(t * 0.3 + phase) * 30 * dpr);
      const cornerY = h * 0.85 + (Math.cos(t * 0.25 + phase * 1.7) * 24 * dpr);
      return [cornerX, cornerY];
    }

    const bs = baseScale(w, h) * viewport.scale;
    const px = w / 2 + (wx * bs) + viewport.offsetX * dpr * viewport.scale + dx;
    const py = h / 2 + (wy * bs) + viewport.offsetY * dpr * viewport.scale + dy;
    return [px, py];
  }

  // ============================================================
  // Rendering
  // ============================================================

  function fitAndRender() {
    if (!canvasEl) return;
    dpr = window.devicePixelRatio || 1;
    const rect = canvasEl.getBoundingClientRect();
    canvasEl.width = Math.max(1, Math.floor(rect.width * dpr));
    canvasEl.height = Math.max(1, Math.floor(rect.height * dpr));
  }

  function drawBackground(ctx: CanvasRenderingContext2D, w: number, h: number) {
    const P = palette;
    const g = ctx.createLinearGradient(0, 0, 0, h);
    g.addColorStop(0, P.bgGradTop);
    g.addColorStop(1, P.bgGradBot);
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, w, h);

    const step = 40 * dpr;
    ctx.lineWidth = 0.5 * dpr;

    ctx.strokeStyle = P.gridMinor;
    for (let x = 0; x <= w; x += step) {
      ctx.beginPath();
      ctx.moveTo(x + 0.5, 0);
      ctx.lineTo(x + 0.5, h);
      ctx.stroke();
    }
    for (let y = 0; y <= h; y += step) {
      ctx.beginPath();
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(w, y + 0.5);
      ctx.stroke();
    }

    ctx.strokeStyle = P.gridMajor;
    for (let x = 0; x <= w; x += step * 4) {
      ctx.beginPath();
      ctx.moveTo(x + 0.5, 0);
      ctx.lineTo(x + 0.5, h);
      ctx.stroke();
    }
    for (let y = 0; y <= h; y += step * 4) {
      ctx.beginPath();
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(w, y + 0.5);
      ctx.stroke();
    }

    ctx.fillStyle = P.ticks;
    const tickStep = 10 * dpr;
    for (let x = 0; x <= w; x += tickStep) {
      const big = (x % (80 * dpr)) === 0;
      const tlen = (big ? 7 : 3) * dpr;
      ctx.fillRect(x, 0, 0.5 * dpr, tlen);
      ctx.fillRect(x, h - tlen, 0.5 * dpr, tlen);
    }
    for (let y = 0; y <= h; y += tickStep) {
      const big = (y % (80 * dpr)) === 0;
      const tlen = (big ? 7 : 3) * dpr;
      ctx.fillRect(0, y, tlen, 0.5 * dpr);
      ctx.fillRect(w - tlen, y, tlen, 0.5 * dpr);
    }

    ctx.strokeStyle = P.crosshair;
    ctx.lineWidth = 0.5 * dpr;
    ctx.setLineDash([4 * dpr, 4 * dpr]);
    ctx.beginPath();
    ctx.moveTo(w / 2, 0);
    ctx.lineTo(w / 2, h);
    ctx.moveTo(0, h / 2);
    ctx.lineTo(w, h / 2);
    ctx.stroke();
    ctx.setLineDash([]);

    if (staleCornerOn) {
      const cx = w * 0.10;
      const cy = h * 0.85;
      const cr = 70 * dpr;
      const grad = ctx.createRadialGradient(cx, cy, 0, cx, cy, cr);
      grad.addColorStop(0, withAlpha(P.ink, 0.05));
      grad.addColorStop(1, withAlpha(P.ink, 0));
      ctx.fillStyle = grad;
      ctx.beginPath();
      ctx.arc(cx, cy, cr, 0, Math.PI * 2);
      ctx.fill();
      ctx.fillStyle = P.inkFaint;
      ctx.font = `500 ${9 * dpr}px "IBM Plex Mono", ui-monospace, monospace`;
      ctx.textAlign = "left";
      ctx.fillText("STALE > 90d", cx - 28 * dpr, cy + cr - 4 * dpr);
    }
  }

  // Convex hull (Phase B): summary containment around children.
  function convexHull(points: [number, number][]): [number, number][] {
    if (points.length < 3) return points.slice();
    const pts = points.slice().sort((a, b) => (a[0] - b[0]) || (a[1] - b[1]));
    const cross = (o: number[], a: number[], b: number[]) =>
      (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    const lower: [number, number][] = [];
    for (const p of pts) {
      while (lower.length >= 2 && cross(lower[lower.length - 2], lower[lower.length - 1], p) <= 0) lower.pop();
      lower.push(p);
    }
    const upper: [number, number][] = [];
    for (let i = pts.length - 1; i >= 0; i--) {
      const p = pts[i];
      while (upper.length >= 2 && cross(upper[upper.length - 2], upper[upper.length - 1], p) <= 0) upper.pop();
      upper.push(p);
    }
    upper.pop();
    lower.pop();
    return lower.concat(upper);
  }

  /** Project a static world-space point to screen — no per-id breathing.
   *  Used for contour centers/sizes so the rings don't wobble. */
  function projectStatic(wx: number, wy: number, w: number, h: number): [number, number] {
    const bs = baseScale(w, h) * viewport.scale;
    const px = w / 2 + (wx * bs) + viewport.offsetX * dpr * viewport.scale;
    const py = h / 2 + (wy * bs) + viewport.offsetY * dpr * viewport.scale;
    return [px, py];
  }

  /** Survey-style concentric contours per cluster. Ring count scales with
   *  mean confidence (1..5). Rings are drawn around the cluster centroid,
   *  sized by std-dev along each axis. */
  function drawClusterContours(
    ctx: CanvasRenderingContext2D,
    w: number, h: number,
  ) {
    if (clusters.length === 0) return;
    const P = palette;
    const bs = baseScale(w, h) * viewport.scale;
    ctx.save();
    for (const c of clusters) {
      if (c.members.length < 2) continue;
      const [cx, cy] = projectStatic(c.cx, c.cy, w, h);
      const rx = Math.max(20 * dpr, Math.abs(c.sx) * bs);
      const ry = Math.max(20 * dpr, Math.abs(c.sy) * bs);
      const rings = Math.max(2, Math.min(5, 1 + Math.floor(c.meanConfidence * 4)));
      for (let i = 0; i < rings; i++) {
        const scale = 1.4 + i * 0.55;
        const a = 0.42 - i * 0.06;  // outer rings fade out
        ctx.beginPath();
        ctx.ellipse(cx, cy, rx * scale, ry * scale, 0, 0, Math.PI * 2);
        ctx.strokeStyle = withAlpha(P.regionRing, Math.max(0.06, a));
        ctx.lineWidth = (i === 0 ? 0.9 : 0.6) * dpr;
        ctx.stroke();
      }
      // Tiny vermilion peak dot at the centroid — visual chop seal for the region.
      ctx.beginPath();
      ctx.arc(cx, cy, 1.4 * dpr, 0, Math.PI * 2);
      ctx.fillStyle = withAlpha(P.accent, 0.55);
      ctx.fill();
      // Region label (uppercase tracked) above the centroid.
      const label = clusterLabels.get(c.id) ?? `Region ${c.id + 1}`;
      const upper = label.toUpperCase();
      ctx.font = `600 ${9 * dpr}px "IBM Plex Sans", system-ui, sans-serif`;
      ctx.fillStyle = withAlpha(P.ink, 0.65);
      ctx.textAlign = "center";
      ctx.textBaseline = "alphabetic";
      // Manual letter-spacing for the tracked-uppercase look.
      const spaced = upper.split("").join(" "); // thin space
      ctx.fillText(spaced, cx, cy - ry * 1.4 - 14 * dpr);
      // Member count + elevation, tiny mono caption beneath the label.
      ctx.font = `400 ${8 * dpr}px "IBM Plex Mono", monospace`;
      ctx.fillStyle = withAlpha(P.ink, 0.40);
      ctx.fillText(
        `${c.members.length} atoms · elev. ${c.meanConfidence.toFixed(2)}`,
        cx, cy - ry * 1.4 - 4 * dpr,
      );
    }
    ctx.restore();
  }

  function drawSummaryHulls(
    ctx: CanvasRenderingContext2D,
    w: number, h: number,
    t: number,
    visible: GraphBelief[],
  ) {
    const P = palette;
    const byId = new Map(visible.map((b) => [b.id, b]));
    for (const [parentId, kids] of summaryChildren) {
      if (!byId.has(parentId)) continue;
      if (kids.length < 2) continue;
      const screenPts: [number, number][] = kids.map((k) =>
        project(k.x as number, k.y as number, k.id, t, w, h, isStale(k)),
      );
      const hull = convexHull(screenPts);
      if (hull.length < 3) continue;
      ctx.save();
      ctx.fillStyle = withAlpha(P.summary, 0.05);
      ctx.strokeStyle = withAlpha(P.summary, 0.30);
      ctx.lineWidth = 0.7 * dpr;
      ctx.beginPath();
      ctx.moveTo(hull[0][0], hull[0][1]);
      for (let i = 1; i < hull.length; i++) ctx.lineTo(hull[i][0], hull[i][1]);
      ctx.closePath();
      ctx.fill();
      ctx.stroke();
      ctx.restore();
    }
  }

  function edgeStyle(kind: GraphEdgeKind, weight: number): {
    color: string; width: number; dash: number[]; arrow: boolean;
  } {
    const P = palette;
    switch (kind) {
      case "hierarchy":
        return { color: P.edgeHierarchy, width: 1.0 * dpr, dash: [], arrow: false };
      case "summarizes":
        return { color: P.edgeSummarizes, width: 1.6 * dpr, dash: [], arrow: false };
      case "reinforced_by":
        return {
          color: P.edgeReinforced,
          width: (0.8 + Math.min(weight, 6) * 0.4) * dpr,
          dash: [],
          arrow: false,
        };
      case "contradicted_by":
        return { color: P.edgeContradicted, width: 1.2 * dpr, dash: [4 * dpr, 4 * dpr], arrow: false };
      case "corrected_by":
        return { color: P.edgeCorrected, width: 1.2 * dpr, dash: [], arrow: true };
      case "knn":
        return { color: P.edgeKnn, width: 0.5 * dpr, dash: [], arrow: false };
      case "co_recall":
        return {
          color: P.edgeCoRecall,
          width: (0.5 + Math.min(weight, 4) * 0.3) * dpr,
          dash: [2 * dpr, 3 * dpr],
          arrow: false,
        };
      case "extracted_from":
        return { color: P.connection, width: 0.4 * dpr, dash: [], arrow: false };
    }
  }

  function drawArrow(
    ctx: CanvasRenderingContext2D,
    sx: number, sy: number, tx: number, ty: number, color: string,
  ) {
    const dx = tx - sx;
    const dy = ty - sy;
    const len = Math.hypot(dx, dy);
    if (len < 1) return;
    const nx = dx / len;
    const ny = dy / len;
    const hx = tx - nx * 6 * dpr;
    const hy = ty - ny * 6 * dpr;
    const px = -ny;
    const py = nx;
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.moveTo(tx, ty);
    ctx.lineTo(hx + px * 3 * dpr, hy + py * 3 * dpr);
    ctx.lineTo(hx - px * 3 * dpr, hy - py * 3 * dpr);
    ctx.closePath();
    ctx.fill();
  }

  // Length-based fade: edges spanning more than ~25% of canvas dim drop
  // toward 15% opacity. Keeps short bonds prominent and long crossings
  // ghosted so the eye isn't pulled across the whole map. Returns multiplier
  // in [0.15, 1].
  function lengthAlpha(len: number, canvasDim: number): number {
    const norm = len / canvasDim;
    if (norm <= 0.25) return 1;
    return Math.max(0.15, 1 - (norm - 0.25) * 1.6);
  }

  function drawEdges(
    ctx: CanvasRenderingContext2D,
    w: number, h: number,
    t: number,
    visible: GraphBelief[],
  ) {
    const byId = new Map(visible.map((b) => [b.id, b]));
    const drawn: ScreenEdge[] = [];
    const canvasDim = Math.min(w, h);

    // Order: faintest first, so important edges land on top.
    const order: GraphEdgeKind[] = [
      "knn", "co_recall", "extracted_from",
      "hierarchy", "reinforced_by", "summarizes",
      "contradicted_by", "corrected_by",
    ];
    const sorted = activeEdges.slice().sort(
      (a, b) => order.indexOf(a.kind) - order.indexOf(b.kind),
    );

    for (const e of sorted) {
      const a = byId.get(e.source_id);
      const b = byId.get(e.target_id);
      if (!a || !b) continue;
      const [sx, sy] = project(a.x as number, a.y as number, a.id, t, w, h, isStale(a));
      const [tx, ty] = project(b.x as number, b.y as number, b.id, t, w, h, isStale(b));

      const dx = tx - sx;
      const dy = ty - sy;
      const len = Math.hypot(dx, dy);

      const style = edgeStyle(e.kind, e.weight);
      let alpha = lengthAlpha(len, canvasDim);
      if (focusReach && !(focusReach.has(e.source_id) && focusReach.has(e.target_id))) {
        alpha *= 0.08;
      }
      if (searchMatchSet) {
        const inHalo =
          searchHaloSet.has(e.source_id) && searchHaloSet.has(e.target_id);
        alpha *= inHalo ? 1 : 0.12;
      }
      if (
        spotlightTurnId &&
        !(spotlightBeliefIds.has(e.source_id) && spotlightBeliefIds.has(e.target_id))
      ) {
        alpha *= 0.18;
      }

      // Subtle perpendicular bow so hub fans don't render as a star of
      // overlapping straight lines. Magnitude scales with length so short
      // edges stay visually straight. kNN/co_recall stay straight (already
      // ambient) — bow only the structural edges.
      const bowed = e.kind !== "knn" && e.kind !== "co_recall" && e.kind !== "extracted_from";
      let cx = (sx + tx) / 2;
      let cy = (sy + ty) / 2;
      if (bowed && len > 1) {
        const nx = -dy / len;
        const ny = dx / len;
        const bow = Math.min(len * 0.10, 60 * dpr);
        cx += nx * bow;
        cy += ny * bow;
      }

      ctx.save();
      ctx.globalAlpha = alpha;
      ctx.strokeStyle = style.color;
      ctx.lineWidth = style.width;
      if (style.dash.length) ctx.setLineDash(style.dash);
      ctx.beginPath();
      ctx.moveTo(sx, sy);
      if (bowed) ctx.quadraticCurveTo(cx, cy, tx, ty);
      else ctx.lineTo(tx, ty);
      ctx.stroke();
      ctx.setLineDash([]);
      if (style.arrow) drawArrow(ctx, sx, sy, tx, ty, style.color);
      ctx.restore();

      drawn.push({ edge: e, sx, sy, tx, ty });
    }

    screenEdges = drawn;
  }

  // ============================================================
  // Labels — keyword-style, with zoom-tier visibility
  // ============================================================

  // Pull a keyword from a belief: prefer the LLM-generated label, otherwise
  // fall back to the first chunk of the statement with framing words stripped.
  function nodeLabel(b: GraphBelief): string {
    if (b.label && b.label.trim()) return b.label.trim();
    const stripped = b.statement
      .replace(/^(The user|The user'?s|The user is|He is|She is|They are|It is|He|She|They|It)\s+/i, "")
      .trim();
    if (stripped.length <= 22) return stripped;
    const cut = stripped.slice(0, 22);
    const lastSpace = cut.lastIndexOf(" ");
    return (lastSpace > 6 ? cut.slice(0, lastSpace) : cut) + "…";
  }

  // Zoom-tier disclosure: summaries always; reinforced leaves at medium zoom;
  // everything else at deep zoom. Hover/search/focus/spotlight always force-on.
  function shouldShowLabel(b: GraphBelief): boolean {
    if (hoverId === b.id) return true;
    if (focusedId === b.id) return true;
    if (searchMatchSet && searchMatchSet.has(b.id)) return true;
    if (spotlightTurnId && spotlightBeliefIds.has(b.id)) return true;
    if (b.trust_class === "summary" || b.level >= 1) return true;
    if (b.reinforced_count >= 3) return viewport.scale >= 1.0;
    return viewport.scale >= 2.2;
  }

  function drawNodeLabel(
    ctx: CanvasRenderingContext2D,
    b: GraphBelief,
    px: number, py: number,
    r: number,
  ) {
    const P = palette;
    const text = nodeLabel(b);
    if (!text) return;
    const isSummary = b.trust_class === "summary" || b.level >= 1;
    const isHi = hoverId === b.id || focusedId === b.id;
    const fontSize = isSummary ? 12 : isHi ? 11 : 10;
    ctx.save();
    ctx.fillStyle = isSummary ? P.ink : P.inkSoft;
    ctx.font = isSummary
      ? `italic 500 ${fontSize * dpr}px "Newsreader", Georgia, serif`
      : `500 ${fontSize * dpr}px "IBM Plex Mono", ui-monospace, monospace`;
    ctx.textAlign = "left";
    ctx.textBaseline = "middle";
    // Backplate behind the text so it stays legible over edges.
    const metrics = ctx.measureText(text);
    const padX = 4 * dpr;
    const padY = 2 * dpr;
    const labelX = px + r + 8 * dpr;
    const labelY = py;
    ctx.fillStyle =
      themeState.current === "light"
        ? "rgba(244,243,238,0.72)"
        : "rgba(10,10,12,0.62)";
    ctx.fillRect(
      labelX - padX,
      labelY - fontSize * dpr * 0.6 - padY,
      metrics.width + padX * 2,
      fontSize * dpr + padY * 2,
    );
    ctx.fillStyle = isSummary ? P.ink : P.inkSoft;
    ctx.fillText(text, labelX, labelY);
    ctx.restore();
  }

  function drawDot(
    ctx: CanvasRenderingContext2D,
    b: GraphBelief,
    px: number, py: number,
    breath: number,
  ) {
    const P = palette;
    const d = ageDays(b);
    let a = recencyAlpha(d) * recencyGlow(d);

    // Search dim
    if (searchMatchSet) {
      if (searchMatchSet.has(b.id)) a = Math.min(1.2, a * 1.1);
      else if (searchHaloSet.has(b.id)) a *= 0.55;
      else a *= 0.18;
    }
    // Focus dim
    if (focusReach && !focusReach.has(b.id)) a *= 0.12;
    // Receipts spotlight
    if (spotlightTurnId) {
      a *= spotlightBeliefIds.has(b.id) ? 1.15 : 0.18;
    }

    if (a < 0.04) return;

    // Status overrides
    const struck = b.status === "corrected";
    const ghost = b.status === "expired";
    const blocked = b.status === "blocked";
    const contested = b.status === "contested";
    if (ghost) a *= 0.45;

    // Trust-class shape + base color
    const trustColor =
      b.trust_class === "asserted" ? P.asserted :
      b.trust_class === "inferred" ? P.inferred :
      b.trust_class === "hypothesized" ? P.hypothesized :
      P.summary;

    // Category hue tints the trust color (Phase B). Mix only a little so trust class still legible.
    const tint = categoryTint(b.category);
    const fillColor = struck ? P.inkFaint : (tint ?? trustColor);

    const conf = b.confidence;
    const r = (1.5 + conf * 1.6) * breath * dpr;

    ctx.save();
    ctx.globalAlpha = Math.min(1, a);

    if (b.trust_class === "hypothesized") {
      ctx.strokeStyle = fillColor;
      ctx.lineWidth = 0.8 * dpr;
      ctx.beginPath();
      ctx.arc(px, py, r, 0, Math.PI * 2);
      ctx.stroke();
    } else if (b.trust_class === "inferred") {
      ctx.fillStyle = fillColor;
      ctx.beginPath();
      ctx.moveTo(px, py - r);
      ctx.lineTo(px + r, py);
      ctx.lineTo(px, py + r);
      ctx.lineTo(px - r, py);
      ctx.closePath();
      ctx.fill();
    } else {
      ctx.fillStyle = fillColor;
      ctx.beginPath();
      ctx.arc(px, py, r, 0, Math.PI * 2);
      ctx.fill();
    }

    // Reinforcement halo (Phase B)
    if (b.reinforced_count > 0) {
      const haloR = r + (3 + Math.min(b.reinforced_count, 6)) * dpr;
      ctx.strokeStyle = withAlpha(P.accent, 0.15 + Math.min(b.reinforced_count, 6) * 0.07);
      ctx.lineWidth = 0.8 * dpr;
      ctx.beginPath();
      ctx.arc(px, py, haloR, 0, Math.PI * 2);
      ctx.stroke();
    }

    // Uncertainty rings (kept from prior implementation)
    const uncertainty = 1 - conf;
    if (uncertainty > 0.05) {
      const rings = 1 + Math.round(uncertainty * 2.5);
      ctx.strokeStyle = withAlpha(fillColor, 0.18 + uncertainty * 0.18);
      ctx.lineWidth = 0.5 * dpr;
      for (let k = 1; k <= rings; k++) {
        ctx.beginPath();
        ctx.arc(
          px, py,
          r + (2 + k * (1.6 + uncertainty * 1.4)) * dpr,
          0, Math.PI * 2,
        );
        ctx.globalAlpha = a * (0.5 - k * 0.1);
        ctx.stroke();
      }
      ctx.globalAlpha = a;
    }

    // Status overlays
    if (struck) {
      ctx.strokeStyle = P.inkSoft;
      ctx.lineWidth = 1 * dpr;
      ctx.beginPath();
      ctx.moveTo(px - r - 2 * dpr, py);
      ctx.lineTo(px + r + 2 * dpr, py);
      ctx.stroke();
    }
    if (contested) {
      const t = performance.now() / 1000;
      const pulse = 0.5 + 0.5 * Math.sin(t * 4);
      ctx.strokeStyle = withAlpha(P.danger, 0.4 + pulse * 0.45);
      ctx.lineWidth = 1.2 * dpr;
      ctx.beginPath();
      ctx.arc(px, py, r + 3 * dpr, 0, Math.PI * 2);
      ctx.stroke();
    }
    if (blocked) {
      ctx.fillStyle = P.danger;
      ctx.font = `bold ${10 * dpr}px ui-sans-serif, system-ui`;
      ctx.textAlign = "center";
      ctx.fillText("⊘", px + r + 5 * dpr, py + 3 * dpr);
    }

    ctx.restore();

    if (shouldShowLabel(b)) {
      drawNodeLabel(ctx, b, px, py, r);
    }
  }

  function drawSummaryGlyph(
    ctx: CanvasRenderingContext2D,
    b: GraphBelief,
    px: number, py: number,
    breath: number,
  ) {
    const P = palette;
    const d = ageDays(b);
    const a = recencyAlpha(d) * recencyGlow(d);
    const r = (5 + Math.min(10, b.reinforced_count) * 0.2) * breath * dpr;

    ctx.save();
    ctx.globalAlpha = Math.min(1, a);
    ctx.fillStyle = P.accent;
    ctx.beginPath();
    ctx.arc(px, py, r, 0, Math.PI * 2);
    ctx.fill();

    ctx.fillStyle = P.summaryCore;
    ctx.beginPath();
    ctx.arc(px, py, r * 0.4, 0, Math.PI * 2);
    ctx.fill();

    ctx.strokeStyle = withAlpha(P.accent, 0.45);
    ctx.lineWidth = 0.7 * dpr;
    ctx.beginPath();
    ctx.arc(px, py, r + 6 * dpr, 0, Math.PI * 2);
    ctx.stroke();

    ctx.beginPath();
    ctx.moveTo(px - r - 11 * dpr, py); ctx.lineTo(px - r - 4 * dpr, py);
    ctx.moveTo(px + r + 4 * dpr, py);  ctx.lineTo(px + r + 11 * dpr, py);
    ctx.moveTo(px, py - r - 11 * dpr); ctx.lineTo(px, py - r - 4 * dpr);
    ctx.moveTo(px, py + r + 4 * dpr);  ctx.lineTo(px, py + r + 11 * dpr);
    ctx.stroke();
    ctx.restore();

    if (shouldShowLabel(b)) {
      drawNodeLabel(ctx, b, px, py, r);
    }
  }

  function drawForeground(
    ctx: CanvasRenderingContext2D,
    w: number, h: number,
    leafCount: number,
  ) {
    const P = palette;
    ctx.save();
    ctx.fillStyle = P.inkFaint;
    ctx.font = `500 ${9 * dpr}px "IBM Plex Mono", ui-monospace, monospace`;

    ctx.textAlign = "left";
    ctx.fillText(
      `CH·1  UMAP·2D  n=${leafCount}  ${activeEdges.length} edges  ${
        scrubberMs ? `as of ${fmtShortDate(scrubberMs)}` : "live"
      }`,
      16 * dpr, 20 * dpr,
    );
    ctx.textAlign = "right";
    ctx.fillText(
      `embed · v${projectionVersion}${focusedId ? "  · FOCUS" : ""}${spotlightTurnId ? "  · SPOTLIGHT" : ""}`,
      w - 16 * dpr, 20 * dpr,
    );
    ctx.textAlign = "left";
    ctx.fillText("subject ⟶ self", 16 * dpr, h - 14 * dpr);
    ctx.textAlign = "right";
    const time = new Date().toLocaleTimeString([], { hour12: false });
    ctx.fillText(time, w - 16 * dpr, h - 14 * dpr);
    ctx.restore();
  }

  function drawHoverRing(ctx: CanvasRenderingContext2D) {
    if (!hoverId) return;
    const hp = screenPoints.find((p) => p.id === hoverId);
    if (!hp) return;
    const P = palette;
    ctx.save();
    ctx.strokeStyle = withAlpha(P.accent, 0.95);
    ctx.lineWidth = 1.5 * dpr;
    ctx.beginPath();
    ctx.arc(hp.sx, hp.sy, hp.r + 4 * dpr, 0, Math.PI * 2);
    ctx.stroke();
    ctx.strokeStyle = withAlpha(P.accent, 0.25);
    ctx.lineWidth = 1 * dpr;
    ctx.beginPath();
    ctx.arc(hp.sx, hp.sy, hp.r + 9 * dpr, 0, Math.PI * 2);
    ctx.stroke();
    ctx.restore();
  }

  function render() {
    if (!canvasEl) return;
    const ctx = canvasEl.getContext("2d");
    if (!ctx) return;
    const w = canvasEl.width;
    const h = canvasEl.height;
    const t = performance.now() / 1000;
    const breath = 1 + 0.04 * Math.sin(t * (Math.PI * 2 / 5));

    ctx.globalCompositeOperation = "source-over";
    ctx.globalAlpha = 1;

    drawBackground(ctx, w, h);

    // Replay tick
    if (replayPlaying && scrubberMs !== null) {
      const now = performance.now();
      if (replayLastFrame === 0) replayLastFrame = now;
      const dt = (now - replayLastFrame) / 1000;
      replayLastFrame = now;
      const next = scrubberMs + dt * REPLAY_DAYS_PER_SEC * 86_400_000;
      if (next >= Date.now()) {
        scrubberMs = Date.now();
        replayPlaying = false;
        replayLastFrame = 0;
      } else {
        scrubberMs = next;
      }
    } else {
      replayLastFrame = 0;
    }

    const visAll = visibleBeliefs;
    const leaves = visAll.filter((b) => b.trust_class !== "summary");
    const summaries = visAll.filter((b) => b.trust_class === "summary");

    // Survey-style cluster contours replace the old summary hulls.
    drawClusterContours(ctx, w, h);
    drawEdges(ctx, w, h, t, visAll);

    const pts: ScreenPoint[] = [];

    for (const b of leaves) {
      const [px, py] = project(
        b.x as number, b.y as number, b.id, t, w, h, isStale(b),
      );
      drawDot(ctx, b, px, py, breath);
      const r = (1.5 + b.confidence * 1.6) * breath * dpr;
      pts.push({ id: b.id, sx: px, sy: py, r });
    }
    for (const b of summaries) {
      const [px, py] = project(
        b.x as number, b.y as number, b.id, t, w, h, isStale(b),
      );
      drawSummaryGlyph(ctx, b, px, py, breath);
      const r = 8 * breath * dpr;
      pts.push({ id: b.id, sx: px, sy: py, r });
    }

    drawHoverRing(ctx);
    drawForeground(ctx, w, h, leaves.length);

    screenPoints = pts;
  }

  // ============================================================
  // Interaction
  // ============================================================

  let dragging = $state(false);
  let lastDrag = { x: 0, y: 0 };

  function onMouseDown(e: MouseEvent) {
    dragging = true;
    lastDrag = { x: e.clientX, y: e.clientY };
  }
  function onMouseUp() {
    dragging = false;
  }
  function onMouseMove(e: MouseEvent) {
    const rect = canvasEl.getBoundingClientRect();
    const cx = (e.clientX - rect.left) * dpr;
    const cy = (e.clientY - rect.top) * dpr;
    if (dragging) {
      viewport.offsetX += (e.clientX - lastDrag.x) / viewport.scale;
      viewport.offsetY += (e.clientY - lastDrag.y) / viewport.scale;
      lastDrag = { x: e.clientX, y: e.clientY };
    } else {
      const id = hitTestPoint(cx, cy);
      if (id !== hoverId) hoverId = id;
      // Edge inspector — only check if not over a node.
      if (!id) {
        const eHit = hitTestEdge(cx, cy);
        if (eHit !== hoveredEdge) hoveredEdge = eHit;
      } else {
        if (hoveredEdge) hoveredEdge = null;
      }
    }
  }
  function onWheel(e: WheelEvent) {
    e.preventDefault();
    const factor = e.deltaY > 0 ? 0.9 : 1.1;
    viewport.scale = Math.max(0.4, Math.min(8, viewport.scale * factor));
  }

  function hitTestPoint(canvasX: number, canvasY: number): string | null {
    let bestId: string | null = null;
    let bestD = Infinity;
    for (const p of screenPoints) {
      const d = Math.hypot(p.sx - canvasX, p.sy - canvasY);
      const limit = Math.max(p.r + 6 * dpr, 14 * dpr);
      if (d < limit && d < bestD) {
        bestD = d;
        bestId = p.id;
      }
    }
    return bestId;
  }

  function distToSegment(
    px: number, py: number, x1: number, y1: number, x2: number, y2: number,
  ): number {
    const dx = x2 - x1;
    const dy = y2 - y1;
    const lenSq = dx * dx + dy * dy;
    if (lenSq === 0) return Math.hypot(px - x1, py - y1);
    let t = ((px - x1) * dx + (py - y1) * dy) / lenSq;
    t = Math.max(0, Math.min(1, t));
    const fx = x1 + t * dx;
    const fy = y1 + t * dy;
    return Math.hypot(px - fx, py - fy);
  }

  function hitTestEdge(canvasX: number, canvasY: number): GraphEdge | null {
    let best: GraphEdge | null = null;
    let bestD = 6 * dpr;
    for (const se of screenEdges) {
      const d = distToSegment(canvasX, canvasY, se.sx, se.sy, se.tx, se.ty);
      if (d < bestD) {
        bestD = d;
        best = se.edge;
      }
    }
    return best;
  }

  async function onClick(e: MouseEvent) {
    const rect = canvasEl.getBoundingClientRect();
    const cx = (e.clientX - rect.left) * dpr;
    const cy = (e.clientY - rect.top) * dpr;
    const id = hitTestPoint(cx, cy);
    if (id) {
      selected = await getBeliefDetail(id);
      // Phase C — receipts spotlight: pull turns this belief was retrieved in.
      try {
        turnsForSelected = await getTurnsForBelief(id);
      } catch (err) {
        turnsForSelected = [];
      }
    } else if (focusedId || spotlightTurnId) {
      // click empty space exits focus / spotlight
      focusedId = null;
      spotlightTurnId = null;
      spotlightBeliefIds = new Set();
    }
  }

  function focusOnSelected() {
    if (!selected) return;
    focusedId = selected.belief.id;
  }

  function clearFocus() {
    focusedId = null;
  }

  function activateSpotlight(turnId: string) {
    spotlightTurnId = turnId;
    // Beliefs co-cited with selected in this turn = all beliefs in that turn's receipts.
    // Use already-fetched turnsForSelected; we don't have the per-turn receipt list locally,
    // so compute the spotlight from co_recall extended edges + selected belief.
    const set = new Set<string>();
    if (selected) set.add(selected.belief.id);
    for (const e of extEdges) {
      if (e.kind !== "co_recall") continue;
      if (e.source_id === selected?.belief.id) set.add(e.target_id);
      if (e.target_id === selected?.belief.id) set.add(e.source_id);
    }
    spotlightBeliefIds = set;
  }

  function clearSpotlight() {
    spotlightTurnId = null;
    spotlightBeliefIds = new Set();
  }

  function resetView() {
    viewport.scale = 1;
    viewport.offsetX = 0;
    viewport.offsetY = 0;
  }

  let hoverBelief = $derived(
    hoverId ? beliefs.find((b) => b.id === hoverId) ?? null : null,
  );

  // Time scrubber bounds.
  let timeBounds = $derived.by(() => {
    if (beliefs.length === 0) return { min: Date.now() - 86_400_000, max: Date.now() };
    let min = Infinity;
    let max = -Infinity;
    for (const b of beliefs) {
      const t = new Date(b.created_at).getTime();
      if (t < min) min = t;
      if (t > max) max = t;
    }
    return { min, max: Math.max(max, Date.now()) };
  });

  function enableTime() {
    timeMode = true;
    if (scrubberMs === null) scrubberMs = Date.now();
  }
  function disableTime() {
    timeMode = false;
    scrubberMs = null;
    replayPlaying = false;
  }
  function startReplay() {
    if (!timeMode) enableTime();
    scrubberMs = timeBounds.min;
    replayPlaying = true;
  }
  function pauseReplay() {
    replayPlaying = false;
  }

  // ============================================================
  // Lifecycle
  // ============================================================

  let rafId: number | null = null;
  let resizeObserver: ResizeObserver | null = null;

  function tick() {
    render();
    rafId = requestAnimationFrame(tick);
  }

  // Toggle: when extended edge filters become true, lazy-load.
  $effect(() => {
    if (
      (edgeFilters.knn || edgeFilters.co_recall) && !extEdgesLoaded
    ) {
      loadExtendedEdges();
    }
  });

  onMount(() => {
    load();
    fitAndRender();
    tick();
    resizeObserver = new ResizeObserver(() => fitAndRender());
    if (canvasEl) resizeObserver.observe(canvasEl);
    const onResize = () => fitAndRender();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  });

  onDestroy(() => {
    if (rafId !== null) cancelAnimationFrame(rafId);
    if (resizeObserver) resizeObserver.disconnect();
  });

  // Edge label helper for inspector tooltip.
  function edgeLabel(k: GraphEdgeKind): string {
    switch (k) {
      case "hierarchy": return "summary parent";
      case "summarizes": return "summarizes";
      case "reinforced_by": return "reinforced by";
      case "contradicted_by": return "contradicts";
      case "corrected_by": return "corrected by";
      case "knn": return "semantically near";
      case "co_recall": return "co-cited in chat";
      case "extracted_from": return "extracted from";
    }
  }

  function statementOf(id: string): string {
    return beliefs.find((b) => b.id === id)?.statement ?? id.slice(0, 8);
  }
</script>

<div
  class="fixed inset-0 z-50 overflow-hidden flex flex-col select-none"
  style="width: 100vw; height: 100vh;
         {themeState.current === 'light'
           ? 'background: #f4f3ee; color: #2a2a2e;'
           : 'background: #0a0a0c; color: #cfd2d8;'}"
>
  <!-- Top bar -->
  <div
    class="px-4 py-2.5 flex items-center justify-between flex-wrap gap-2"
    style="border-bottom: 1px solid {themeState.current === 'light'
      ? 'rgba(42,42,46,0.12)'
      : 'rgba(207,210,216,0.10)'};"
  >
    <div class="flex items-center gap-3 min-w-0">
      <h2 class="text-sm font-semibold tracking-wide whitespace-nowrap">
        Memory map
      </h2>
      <span
        class="text-[11px] truncate"
        style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
               color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(207,210,216,0.50)'};"
      >
        {status}
      </span>
      <input
        type="text"
        placeholder="search beliefs…"
        bind:value={searchQuery}
        class="ml-2 px-2 py-1 text-[11px] outline-none w-44"
        style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
               background: {themeState.current === 'light' ? 'rgba(42,42,46,0.05)' : 'rgba(207,210,216,0.06)'};
               border: 1px solid {themeState.current === 'light' ? 'rgba(42,42,46,0.15)' : 'rgba(207,210,216,0.10)'};
               border-radius: 4px;
               color: inherit;"
      />
    </div>

    <div class="flex items-center gap-3 text-[11px] relative"
         style="font-family: 'IBM Plex Mono', ui-monospace, monospace;">
      <!-- Trust class quick-toggles. The four most-used filters get top-level chips. -->
      <label class="flex items-center gap-1.5 cursor-pointer">
        <input type="checkbox" bind:checked={filterAsserted} />
        <span class="inline-block w-2 h-2 rounded-full" style="background: {palette.asserted};"></span>
        asserted
      </label>
      <label class="flex items-center gap-1.5 cursor-pointer">
        <input type="checkbox" bind:checked={filterInferred} />
        <span class="inline-block w-2 h-2" style="background: {palette.inferred}; transform: rotate(45deg);"></span>
        inferred
      </label>
      <label class="flex items-center gap-1.5 cursor-pointer">
        <input type="checkbox" bind:checked={filterHypothesized} />
        <span class="inline-block w-2 h-2 rounded-full" style="border: 1px solid {palette.hypothesized};"></span>
        hyp.
      </label>
      <label class="flex items-center gap-1.5 cursor-pointer">
        <input type="checkbox" bind:checked={filterSummary} />
        <span class="inline-block w-2 h-2 rounded-full" style="background: {palette.summary};"></span>
        summary
      </label>

      <span style="opacity: 0.4;">|</span>

      <button
        onclick={timeMode ? disableTime : enableTime}
        class="opacity-60 hover:opacity-100"
        class:active={timeMode}
        title={timeMode ? "Back to live view" : "Scrub through the ledger's history"}
      >{timeMode ? "● live" : "⌛ time"}</button>
      <button onclick={resetView} class="opacity-60 hover:opacity-100" title="Reset zoom + pan">⊕</button>
      <button
        onclick={() => (advancedOpen = !advancedOpen)}
        class="opacity-60 hover:opacity-100"
        title="More controls"
        aria-expanded={advancedOpen}
      >⋯</button>
      <button onclick={onClose} class="opacity-60 hover:opacity-100 text-base leading-none" aria-label="Close">×</button>

      {#if advancedOpen}
        <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
        <div
          class="absolute right-0 top-full mt-2 z-30 w-72 rounded-md shadow-lg p-3 text-[11px] space-y-3"
          style="background: var(--pal-surface); border: 1px solid var(--pal-border); color: var(--pal-ink);"
          onclick={(e) => e.stopPropagation()}
        >
          <div>
            <div class="text-[10px] uppercase tracking-wider opacity-50 mb-1.5">Edges</div>
            <div class="grid grid-cols-2 gap-1.5">
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.hierarchy} />
                <span class="inline-block w-3" style="border-top: 1.5px solid {palette.edgeHierarchy};"></span>
                hierarchy
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.summarizes} />
                <span class="inline-block w-3" style="border-top: 2px solid {palette.edgeSummarizes};"></span>
                summarizes
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.reinforced_by} />
                <span class="inline-block w-3" style="border-top: 2px solid {palette.edgeReinforced};"></span>
                reinforced
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.corrected_by} />
                <span class="inline-block w-3" style="border-top: 1.5px solid {palette.edgeCorrected};"></span>
                corrected
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.contradicted_by} />
                <span class="inline-block w-3" style="border-top: 1.5px dashed {palette.edgeContradicted};"></span>
                contradicts
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.knn} />
                <span class="inline-block w-3" style="border-top: 1px solid {palette.edgeKnn};"></span>
                kNN
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={edgeFilters.co_recall} />
                <span class="inline-block w-3" style="border-top: 1px dashed {palette.edgeCoRecall};"></span>
                co-recall
              </label>
            </div>
          </div>

          <div>
            <div class="text-[10px] uppercase tracking-wider opacity-50 mb-1.5">Status overlays</div>
            <div class="grid grid-cols-2 gap-1.5">
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={showCorrected} />
                corrected
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={showContested} />
                contested
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={showExpired} />
                expired
              </label>
              <label class="flex items-center gap-1.5 cursor-pointer">
                <input type="checkbox" bind:checked={showBlocked} />
                blocked
              </label>
            </div>
          </div>

          <div class="flex items-center justify-between pt-1 border-t" style="border-color: var(--pal-border);">
            <label class="flex items-center gap-1.5 cursor-pointer">
              <input type="checkbox" bind:checked={staleCornerOn} />
              stale corner
            </label>
            <button
              onclick={() => (legendOpen = !legendOpen)}
              class="opacity-70 hover:opacity-100"
            >legend ?</button>
            <button
              onclick={backfillLabels}
              disabled={labeling}
              class="opacity-70 hover:opacity-100"
            >{labeling ? "labeling…" : "label all"}</button>
          </div>
        </div>
      {/if}
    </div>
  </div>

  <!-- Time scrubber. As-of label + replay speed indicator + jump-to-live. -->
  {#if timeMode}
    <div
      class="px-4 py-2 flex items-center gap-3 text-[10px] pal-dim"
      style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
             border-bottom: 1px solid var(--pal-border);"
    >
      <button
        onclick={replayPlaying ? pauseReplay : startReplay}
        class="opacity-70 hover:opacity-100"
        title={replayPlaying
          ? `Pause (replaying ~${REPLAY_DAYS_PER_SEC}d/s)`
          : `Replay from start (~${REPLAY_DAYS_PER_SEC}d/s)`}
      >{replayPlaying ? "⏸" : "▶"}</button>
      <span>{fmtShortDate(timeBounds.min)}</span>
      <input
        type="range"
        min={timeBounds.min}
        max={timeBounds.max}
        step={3600_000}
        value={scrubberMs ?? timeBounds.max}
        oninput={(e) => {
          scrubberMs = +(e.currentTarget as HTMLInputElement).value;
          replayPlaying = false;
        }}
        class="flex-1"
        aria-label="Scrub the belief ledger through time"
      />
      <span>{fmtShortDate(timeBounds.max)}</span>
      {#if replayPlaying}
        <span class="pal-accent-text" title="Replay speed">~{REPLAY_DAYS_PER_SEC}d/s</span>
      {/if}
      <span style="opacity: 0.85;">
        {#if scrubberMs}
          as of <span class="pal-accent-text">{fmtShortDate(scrubberMs)}</span>
          <span class="opacity-60">({fmtRelative(scrubberMs)})</span>
        {:else}
          <span class="pal-accent-text">live</span>
        {/if}
      </span>
      {#if scrubberMs}
        <button
          onclick={() => {
            scrubberMs = null;
            replayPlaying = false;
          }}
          class="opacity-60 hover:opacity-100"
          title="Jump back to live"
        >↺ live</button>
      {/if}
    </div>
  {/if}

  <!-- Canvas + side detail -->
  <div class="flex-1 flex overflow-hidden relative">
    <div class="flex-1 relative overflow-hidden">
      <canvas
        bind:this={canvasEl}
        onmousedown={onMouseDown}
        onmouseup={onMouseUp}
        onmouseleave={() => {
          dragging = false;
          hoverId = null;
          hoveredEdge = null;
        }}
        onmousemove={onMouseMove}
        onwheel={onWheel}
        onclick={onClick}
        class="block w-full h-full {dragging ? 'cursor-grabbing' : 'cursor-crosshair'}"
      ></canvas>

      {#if loading || projecting}
        <div class="absolute inset-0 flex items-center justify-center pointer-events-none">
          <div
            class="px-4 py-2 text-xs"
            style="background: {themeState.current === 'light'
              ? 'rgba(244,243,238,0.85)'
              : 'rgba(20,18,14,0.85)'};
              color: {themeState.current === 'light' ? '#2a2a2e' : '#e8dccb'};
              border: 1px solid {themeState.current === 'light' ? 'rgba(42,42,46,0.18)' : 'rgba(232,220,203,0.18)'};
              backdrop-filter: blur(8px);
              font-family: 'IBM Plex Mono', ui-monospace, monospace;"
          >
            {projecting ? "Projecting…" : "Loading…"}
          </div>
        </div>
      {/if}

      <!-- Hover belief tooltip -->
      {#if hoverBelief}
        <div
          class="absolute bottom-4 left-1/2 -translate-x-1/2 max-w-2xl px-4 py-3 pointer-events-none"
          style="background: {themeState.current === 'light'
            ? 'rgba(244,243,238,0.94)'
            : 'rgba(20,18,14,0.94)'};
            color: {themeState.current === 'light' ? '#2a2a2e' : '#e8dccb'};
            border: 1px solid {themeState.current === 'light'
              ? 'rgba(42,42,46,0.18)'
              : 'rgba(232,220,203,0.18)'};
            border-radius: 8px;
            box-shadow: 0 1px 0 rgba(255,255,255,0.04) inset, 0 12px 32px rgba(0,0,0,0.45);
            backdrop-filter: blur(14px) saturate(140%);"
        >
          <div
            class="flex items-center gap-2 mb-1"
            style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                   font-size: 9px;
                   letter-spacing: 0.10em;
                   text-transform: uppercase;
                   color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(232,220,203,0.55)'};"
          >
            <span
              class="inline-block w-[7px] h-[7px] rounded-full"
              style="background: {hoverBelief.trust_class === 'asserted'
                ? palette.asserted
                : hoverBelief.trust_class === 'inferred'
                  ? palette.inferred
                  : hoverBelief.trust_class === 'summary'
                    ? palette.summary
                    : palette.hypothesized};"
            ></span>
            {hoverBelief.trust_class} · {hoverBelief.status}{#if hoverBelief.category} · {hoverBelief.category}{/if}
          </div>
          <p class="text-sm leading-snug" style="font-weight: 500; letter-spacing: -0.005em;">
            "{hoverBelief.statement}"
          </p>
          <p
            class="mt-1.5 flex gap-2.5 flex-wrap"
            style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                   font-size: 9px;
                   color: {themeState.current === 'light' ? 'rgba(42,42,46,0.50)' : 'rgba(232,220,203,0.45)'};"
          >
            <span>{hoverBelief.confidence_bucket}</span>
            {#if hoverBelief.reinforced_count > 0}
              <span>×{hoverBelief.reinforced_count} reinforced</span>
            {/if}
            <span>{recencyLabel(ageDays(hoverBelief))}</span>
          </p>
        </div>
      {/if}

      <!-- Edge inspector tooltip (Phase C) -->
      {#if hoveredEdge && !hoverBelief}
        <div
          class="absolute top-4 left-1/2 -translate-x-1/2 max-w-xl px-3 py-2 pointer-events-none"
          style="background: {themeState.current === 'light'
            ? 'rgba(244,243,238,0.94)'
            : 'rgba(20,18,14,0.94)'};
            color: {themeState.current === 'light' ? '#2a2a2e' : '#e8dccb'};
            border: 1px solid {themeState.current === 'light' ? 'rgba(42,42,46,0.18)' : 'rgba(232,220,203,0.18)'};
            border-radius: 6px;
            font-family: 'IBM Plex Mono', ui-monospace, monospace;
            font-size: 11px;"
        >
          <div style="opacity: 0.7;">{edgeLabel(hoveredEdge.kind)} · weight {hoveredEdge.weight.toFixed(2)}</div>
          <div class="mt-0.5 truncate">"{statementOf(hoveredEdge.source_id)}"</div>
          <div class="truncate" style="opacity: 0.65;">↳ "{statementOf(hoveredEdge.target_id)}"</div>
        </div>
      {/if}

      <!-- Legend overlay (Phase B) -->
      {#if legendOpen}
        <div
          class="absolute top-4 right-4 w-[280px] p-3 text-[11px]"
          style="background: {themeState.current === 'light'
            ? 'rgba(244,243,238,0.97)'
            : 'rgba(10,10,12,0.97)'};
            color: inherit;
            border: 1px solid {themeState.current === 'light' ? 'rgba(42,42,46,0.18)' : 'rgba(232,220,203,0.18)'};
            border-radius: 6px;
            font-family: 'IBM Plex Mono', ui-monospace, monospace;"
        >
          <div class="flex items-center justify-between mb-2">
            <strong>Legend</strong>
            <button onclick={() => (legendOpen = false)} class="opacity-60 hover:opacity-100">×</button>
          </div>
          <div class="mb-1.5" style="opacity: 0.7;">SHAPE = trust class</div>
          <div class="grid grid-cols-2 gap-y-1 gap-x-2 mb-2">
            <div>● asserted</div>
            <div>◆ inferred</div>
            <div>○ hypothesized</div>
            <div>✦ summary</div>
          </div>
          <div class="mb-1.5" style="opacity: 0.7;">HUE = category</div>
          <div class="grid grid-cols-2 gap-y-1 gap-x-2 mb-2">
            {#each Object.entries(CATEGORY_HUE) as [name, _]}
              <div class="flex items-center gap-1.5">
                <span class="inline-block w-2 h-2 rounded-full" style="background: {categoryTint(name)};"></span>
                {name}
              </div>
            {/each}
          </div>
          <div class="mb-1.5" style="opacity: 0.7;">STATUS</div>
          <div class="space-y-0.5 mb-2">
            <div>strikethrough = corrected</div>
            <div>red ring (pulse) = contested</div>
            <div>20% opacity = expired</div>
            <div>⊘ glyph = blocked</div>
          </div>
          <div class="mb-1.5" style="opacity: 0.7;">EDGES</div>
          <div class="space-y-0.5 mb-2">
            <div>solid = hierarchy / reinforced</div>
            <div>thick gold = summarizes</div>
            <div>red dashed = contradicts</div>
            <div>orange + arrow = corrected_by</div>
            <div>faint = kNN / co-recall</div>
          </div>
          <div style="opacity: 0.7;">RING SIZE = uncertainty (1−conf)</div>
          <div style="opacity: 0.7;">GOLD HALO = reinforcement count</div>
        </div>
      {/if}
    </div>

    <!-- Side detail drawer -->
    {#if selected}
      <aside
        class="w-[420px] shrink-0 overflow-y-auto p-4"
        style="background: {themeState.current === 'light'
          ? 'rgba(244,243,238,0.96)'
          : 'rgba(10,10,12,0.96)'};
          border-left: 1px solid {themeState.current === 'light'
            ? 'rgba(42,42,46,0.12)'
            : 'rgba(207,210,216,0.10)'};
          color: {themeState.current === 'light' ? '#2a2a2e' : '#cfd2d8'};
          backdrop-filter: blur(8px);"
      >
        <div class="flex items-start justify-between mb-3">
          <h3 class="text-sm font-semibold">Belief detail</h3>
          <div class="flex gap-2">
            {#if focusedId === selected.belief.id}
              <button
                onclick={clearFocus}
                class="text-[10px] opacity-70 hover:opacity-100 px-2 py-0.5"
                style="border: 1px solid currentColor; border-radius: 3px;"
              >× focus</button>
            {:else}
              <button
                onclick={focusOnSelected}
                class="text-[10px] opacity-70 hover:opacity-100 px-2 py-0.5"
                style="border: 1px solid currentColor; border-radius: 3px;"
              >focus 2-hop</button>
            {/if}
            <button
              onclick={() => {
                selected = null;
                turnsForSelected = [];
                clearSpotlight();
              }}
              class="opacity-60 hover:opacity-100"
              aria-label="Close detail"
            >×</button>
          </div>
        </div>

        <p class="text-sm">{selected.belief.statement}</p>
        <p
          class="mt-1"
          style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                 font-size: 11px;
                 color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(207,210,216,0.55)'};"
        >
          {selected.belief.trust_class} · {selected.belief.status}{#if selected.belief.category} · {selected.belief.category}{/if}
          · {selected.belief.confidence_bucket}
        </p>

        <!-- Receipts spotlight (Phase C) -->
        {#if turnsForSelected.length > 0}
          <div class="mt-4">
            <div
              class="flex items-center justify-between mb-1.5 text-[10px]"
              style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                     opacity: 0.65; text-transform: uppercase; letter-spacing: 0.08em;"
            >
              <span>Cited in {turnsForSelected.length} turn{turnsForSelected.length === 1 ? '' : 's'}</span>
              {#if spotlightTurnId}
                <button onclick={clearSpotlight} class="hover:opacity-100">× clear spotlight</button>
              {/if}
            </div>
            <div class="space-y-1">
              {#each turnsForSelected.slice(0, 8) as t (t.turn_id)}
                <button
                  onclick={() => activateSpotlight(t.turn_id)}
                  class="w-full text-left p-2 text-xs"
                  style="background: {spotlightTurnId === t.turn_id
                    ? (themeState.current === 'light' ? 'rgba(178,82,28,0.12)' : 'rgba(255,181,71,0.10)')
                    : (themeState.current === 'light' ? 'rgba(42,42,46,0.04)' : 'rgba(207,210,216,0.04)')};
                    border: 1px solid {spotlightTurnId === t.turn_id
                      ? palette.accent
                      : (themeState.current === 'light' ? 'rgba(42,42,46,0.10)' : 'rgba(207,210,216,0.08)')};
                    border-radius: 4px;
                    cursor: pointer;"
                >
                  <div
                    style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                           font-size: 9px; opacity: 0.55;"
                  >
                    rank #{t.rank + 1} · {t.created_at.slice(0, 10)} · {t.conversation_title}
                  </div>
                  <div class="line-clamp-2 mt-0.5">{t.preview}…</div>
                </button>
              {/each}
            </div>
          </div>
        {/if}

        <div class="mt-4 space-y-2 text-xs">
          {#each selected.versions.slice().reverse() as v (v.version_num)}
            <div
              class="p-2 rounded"
              style="background: {themeState.current === 'light'
                ? 'rgba(42,42,46,0.05)'
                : 'rgba(207,210,216,0.04)'};
                border: 1px solid {themeState.current === 'light'
                  ? 'rgba(42,42,46,0.10)'
                  : 'rgba(207,210,216,0.08)'};"
            >
              <div
                class="flex items-center justify-between"
                style="font-family: 'IBM Plex Mono', ui-monospace, monospace;
                       font-size: 10px;
                       color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(207,210,216,0.55)'};"
              >
                <span>v{v.version_num} · {v.editor}</span>
              </div>
              <p class="mt-0.5">{v.statement}</p>
              {#if v.reason}
                <p
                  class="italic mt-0.5"
                  style="color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(207,210,216,0.55)'};"
                >
                  "{v.reason}"
                </p>
              {/if}
              {#if v.provenance.length > 0}
                <ul class="mt-1.5 space-y-1">
                  {#each v.provenance as p (p.source_id + p.relation)}
                    <li
                      class="pl-2"
                      style="border-left: 2px solid {palette.accent};
                             font-size: 11px;
                             color: {themeState.current === 'light' ? 'rgba(42,42,46,0.55)' : 'rgba(207,210,216,0.55)'};"
                    >
                      <span style="color: {palette.accent}; font-weight: 500;">{p.relation}</span>
                      <span style="opacity: 0.6;">({p.source_type})</span>{#if p.preview}: <span class="italic">{p.preview}</span>{/if}
                    </li>
                  {/each}
                </ul>
              {/if}
            </div>
          {/each}
        </div>
      </aside>
    {/if}
  </div>
</div>
