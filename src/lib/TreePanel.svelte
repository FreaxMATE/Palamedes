<script lang="ts">
  import type { Message } from "./chat";
  import Icon from "./Icon.svelte";

  interface Props {
    messages: Message[];
    currentPathIds: Set<string>;
    currentLeafId: string | null;
    onSelect: (messageId: string) => void;
    onClose: () => void;
  }

  let { messages, currentPathIds, currentLeafId, onSelect, onClose }: Props =
    $props();

  const LANE_W = 18;
  const ROW_H = 30;
  const NODE_R = 5;
  const PAD_X = 16;
  const PAD_Y = 14;

  interface Laid {
    msg: Message;
    lane: number;
    row: number;
  }

  // Deterministic layout: DFS from roots. First child keeps parent's lane,
  // later children get a fresh lane (never reused — simple & readable for small trees).
  function layout(all: Message[]): {
    rows: Laid[];
    maxLane: number;
    childrenOf: Map<string, string[]>;
    byId: Map<string, Laid>;
  } {
    const byParent = new Map<string | null, Message[]>();
    for (const m of all) {
      const k = m.parent_id;
      if (!byParent.has(k)) byParent.set(k, []);
      byParent.get(k)!.push(m);
    }
    for (const list of byParent.values()) {
      list.sort((a, b) => a.created_at.localeCompare(b.created_at));
    }

    const rows: Laid[] = [];
    const byId = new Map<string, Laid>();
    const childrenOf = new Map<string, string[]>();
    let nextLane = 0;
    let row = 0;

    function visit(msg: Message, lane: number) {
      const laid: Laid = { msg, lane, row };
      rows.push(laid);
      byId.set(msg.id, laid);
      row++;
      const children = byParent.get(msg.id) ?? [];
      childrenOf.set(msg.id, children.map((c) => c.id));
      children.forEach((c, i) => {
        const childLane = i === 0 ? lane : ++nextLane;
        visit(c, childLane);
      });
    }

    const roots = byParent.get(null) ?? [];
    roots.forEach((r, i) => {
      const rootLane = i === 0 ? 0 : ++nextLane;
      visit(r, rootLane);
    });

    return { rows, maxLane: nextLane, childrenOf, byId };
  }

  let { rows, maxLane, childrenOf, byId } = $derived.by(() => layout(messages));
  let width = $derived(PAD_X * 2 + (maxLane + 1) * LANE_W + 260);
  let height = $derived(PAD_Y * 2 + rows.length * ROW_H);
  let labelX = $derived(PAD_X + (maxLane + 1) * LANE_W + 10);

  function laneX(lane: number) {
    return PAD_X + lane * LANE_W + NODE_R;
  }
  function rowY(row: number) {
    return PAD_Y + row * ROW_H + ROW_H / 2;
  }

  function connectorPath(parent: Laid, child: Laid): string {
    const x1 = laneX(parent.lane);
    const y1 = rowY(parent.row);
    const x2 = laneX(child.lane);
    const y2 = rowY(child.row);
    if (x1 === x2) {
      return `M ${x1} ${y1} L ${x2} ${y2}`;
    }
    // Step with rounded corner at the transition row.
    const midY = y2 - ROW_H / 2;
    const r = Math.min(Math.abs(x2 - x1), ROW_H / 2) * 0.8;
    const dir = x2 > x1 ? 1 : -1;
    return (
      `M ${x1} ${y1} ` +
      `L ${x1} ${midY - r} ` +
      `Q ${x1} ${midY} ${x1 + dir * r} ${midY} ` +
      `L ${x2 - dir * r} ${midY} ` +
      `Q ${x2} ${midY} ${x2} ${midY + r} ` +
      `L ${x2} ${y2}`
    );
  }

  function preview(m: Message): string {
    if (m.branch_title) return m.branch_title;
    const first = m.content.split(/\n/)[0] ?? "";
    return first.length > 36 ? first.slice(0, 36) + "…" : first || "(empty)";
  }

  function rolePrefix(m: Message): string {
    return m.role === "user" ? "U" : m.role === "assistant" ? "A" : "S";
  }
</script>

<aside class="tree-aside flex flex-col">
  <div
    class="px-4 py-3 flex items-center justify-between"
    style="border-bottom: 1px solid var(--pal-border);"
  >
    <h2 class="wordmark" style="color: var(--pal-ink);">Branches</h2>
    <button class="tree-icon" onclick={onClose} aria-label="Close branches panel">
      <Icon name="close" size={14} label="close" />
    </button>
  </div>

  <div class="flex-1 overflow-auto p-2">
    {#if rows.length === 0}
      <p class="text-sm p-2" style="color: var(--pal-dim);">No messages yet.</p>
    {:else}
      <svg
        {width}
        {height}
        class="block"
        xmlns="http://www.w3.org/2000/svg"
      >
        <!-- Connector paths first (behind nodes) -->
        {#each rows as laid (laid.msg.id)}
          {#each childrenOf.get(laid.msg.id) ?? [] as childId (childId)}
            {@const child = byId.get(childId)}
            {#if child}
              {@const onPath =
                currentPathIds.has(laid.msg.id) && currentPathIds.has(child.msg.id)}
              <path
                class="conn"
                class:on-path={onPath}
                d={connectorPath(laid, child)}
              />
            {/if}
          {/each}
        {/each}

        <!-- Nodes + labels -->
        {#each rows as laid (laid.msg.id)}
          {@const onPath = currentPathIds.has(laid.msg.id)}
          {@const isLeaf = laid.msg.id === currentLeafId}
          {@const cx = laneX(laid.lane)}
          {@const cy = rowY(laid.row)}
          <g
            class="tree-row"
            class:on-path={onPath}
            onclick={() => onSelect(laid.msg.id)}
            onkeydown={(e) => e.key === "Enter" && onSelect(laid.msg.id)}
            role="button"
            tabindex="0"
          >
            <!-- Full-row hover/focus target. Hover is driven from the <g> so
                 hovering the label text highlights the row too. -->
            <rect
              class="row-bg"
              x={4}
              y={cy - ROW_H / 2 + 1}
              width={Math.max(0, width - 8)}
              height={ROW_H - 2}
              rx={7}
            />
            {#if isLeaf}
              <circle class="halo" {cx} {cy} r={NODE_R + 4} />
            {/if}
            <circle
              class="node"
              class:on-path={onPath}
              class:leaf={isLeaf}
              {cx}
              {cy}
              r={isLeaf ? NODE_R + 1.5 : NODE_R}
            />
            <text class="role-glyph num" x={labelX} y={cy + 4}>
              {rolePrefix(laid.msg)}
            </text>
            <text class="preview" class:on-path={onPath} x={labelX + 14} y={cy + 4}>
              {preview(laid.msg)}
            </text>
          </g>
        {/each}
      </svg>
    {/if}
  </div>
</aside>

<style>
  .tree-aside {
    width: 20rem;
    flex-shrink: 0;
    height: 100%;
    background: var(--pal-bg-sunken);
    border-left: 1px solid var(--pal-border);
    box-shadow: var(--pal-shadow-lg);
  }

  /* Close button — mirrors the audit panel's icon button. */
  .tree-icon {
    background: transparent;
    color: var(--pal-dim);
    border: 1px solid transparent;
    border-radius: var(--pal-radius);
    padding: 2px 6px;
    line-height: 1;
    cursor: pointer;
    transition: color 120ms ease, border-color 120ms ease;
  }
  .tree-icon:hover {
    color: var(--pal-ink);
    border-color: var(--pal-border);
  }

  /* Row group: pointer + a calm hover wash behind the whole row. Because the
     fill lives on the <g>, hovering the label text triggers it as well. */
  .tree-row {
    cursor: pointer;
  }
  .row-bg {
    fill: transparent;
    transition: fill 120ms ease;
  }
  .tree-row:hover .row-bg {
    fill: rgba(0, 0, 0, 0.05);
  }
  :global(.dark) .tree-row:hover .row-bg {
    fill: rgba(255, 255, 255, 0.04);
  }
  .tree-row:focus-visible {
    outline: none;
  }
  .tree-row:focus-visible .row-bg {
    fill: var(--pal-accent-soft);
  }

  /* Connectors: the active path is vermilion; everything else recedes into
     muted ink so the current branch is unmistakable. */
  .conn {
    fill: none;
    stroke: var(--pal-dim);
    stroke-width: 1.25;
    stroke-opacity: 0.45;
  }
  .conn.on-path {
    stroke: rgb(var(--pal-accent));
    stroke-width: 2.25;
    stroke-opacity: 1;
  }

  /* Nodes: hollow on the canvas off-path, filled vermilion on the path. */
  .node {
    fill: var(--pal-bg-sunken);
    stroke: var(--pal-dim);
    stroke-width: 1.75;
  }
  .node.on-path {
    fill: rgb(var(--pal-accent));
    stroke: rgb(var(--pal-accent));
  }
  .node.leaf {
    stroke-width: 2.5;
  }
  .halo {
    fill: none;
    stroke: rgb(var(--pal-accent));
    stroke-opacity: 0.35;
    stroke-width: 1.5;
  }

  .role-glyph {
    fill: var(--pal-dim);
    font-size: 10px;
    opacity: 0.7;
    user-select: none;
  }
  .preview {
    fill: var(--pal-dim);
    font-size: 12px;
    user-select: none;
  }
  .preview.on-path {
    fill: var(--pal-ink);
    font-weight: 500;
  }
</style>
