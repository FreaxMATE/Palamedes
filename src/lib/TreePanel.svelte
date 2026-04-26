<script lang="ts">
  import type { Message } from "./chat";

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
  const ROW_H = 28;
  const NODE_R = 5;
  const PAD_X = 14;
  const PAD_Y = 14;

  const LANE_COLORS = [
    "#8b5cf6", // violet
    "#06b6d4", // cyan
    "#f59e0b", // amber
    "#10b981", // emerald
    "#ec4899", // pink
    "#6366f1", // indigo
    "#ef4444", // red
    "#14b8a6", // teal
  ];

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

  function laneX(lane: number) {
    return PAD_X + lane * LANE_W + NODE_R;
  }
  function rowY(row: number) {
    return PAD_Y + row * ROW_H + ROW_H / 2;
  }
  function laneColor(lane: number) {
    return LANE_COLORS[lane % LANE_COLORS.length];
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

<aside
  class="w-80 shrink-0 border-l border-neutral-200 dark:border-neutral-800 flex flex-col bg-white dark:bg-neutral-950"
>
  <div
    class="p-3 border-b border-neutral-200 dark:border-neutral-800 flex items-center justify-between"
  >
    <h2 class="text-sm font-semibold">Branches</h2>
    <button
      onclick={onClose}
      class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 text-sm"
      aria-label="Close branches panel"
    >
      ×
    </button>
  </div>

  <div class="flex-1 overflow-auto p-2">
    {#if rows.length === 0}
      <p class="text-sm text-neutral-500 p-2">No messages yet.</p>
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
                d={connectorPath(laid, child)}
                stroke={laneColor(child.lane)}
                stroke-width={onPath ? 2.25 : 1.25}
                stroke-opacity={onPath ? 1 : 0.55}
                fill="none"
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
          {@const labelX = PAD_X + (maxLane + 1) * LANE_W + 8}
          <g
            class="cursor-pointer"
            onclick={() => onSelect(laid.msg.id)}
            onkeydown={(e) => e.key === "Enter" && onSelect(laid.msg.id)}
            role="button"
            tabindex="0"
          >
            <!-- Full-row hover target -->
            <rect
              x={0}
              y={cy - ROW_H / 2}
              width={width}
              height={ROW_H}
              class="fill-transparent hover:fill-neutral-100 dark:hover:fill-neutral-900"
            />
            <circle
              {cx}
              {cy}
              r={isLeaf ? NODE_R + 1.5 : NODE_R}
              fill={onPath ? laneColor(laid.lane) : "white"}
              stroke={laneColor(laid.lane)}
              stroke-width={isLeaf ? 2.5 : 1.75}
              class="dark:[&:not([fill=white])]:fill-inherit"
            />
            {#if isLeaf}
              <circle
                {cx}
                {cy}
                r={NODE_R + 4}
                fill="none"
                stroke={laneColor(laid.lane)}
                stroke-opacity="0.35"
                stroke-width="1.5"
              />
            {/if}
            <text
              x={labelX}
              y={cy + 4}
              class="text-[11px] fill-neutral-500 dark:fill-neutral-500 select-none"
            >
              {rolePrefix(laid.msg)}
            </text>
            <text
              x={labelX + 12}
              y={cy + 4}
              class="text-[12px] select-none {onPath
                ? 'fill-neutral-900 dark:fill-neutral-100 font-medium'
                : 'fill-neutral-500 dark:fill-neutral-500'}"
            >
              {preview(laid.msg)}
            </text>
          </g>
        {/each}
      </svg>
    {/if}
  </div>
</aside>
