<script lang="ts">
  import type { Conversation } from "./chat";

  interface Props {
    conversations: Conversation[];
    activeId: string | null;
    onSelect: (id: string) => void;
    onNew: () => void;
    onDelete: (id: string) => void;
    onOpenSettings: () => void;
  }

  let { conversations, activeId, onSelect, onNew, onDelete, onOpenSettings }: Props =
    $props();

  type Bucket = "Today" | "Yesterday" | "This week" | "This month" | "Older";

  function bucketFor(iso: string): Bucket {
    const d = new Date(iso);
    const now = new Date();
    const startOfToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    const ms = startOfToday - d.getTime();
    const day = 86_400_000;
    if (d.getTime() >= startOfToday) return "Today";
    if (ms < day) return "Yesterday";
    if (ms < 7 * day) return "This week";
    if (ms < 30 * day) return "This month";
    return "Older";
  }

  let grouped = $derived.by(() => {
    const order: Bucket[] = ["Today", "Yesterday", "This week", "This month", "Older"];
    const out = new Map<Bucket, Conversation[]>();
    for (const b of order) out.set(b, []);
    for (const c of conversations) out.get(bucketFor(c.updated_at))!.push(c);
    return order
      .map((b) => ({ bucket: b, items: out.get(b)! }))
      .filter((g) => g.items.length > 0);
  });
</script>

<aside
  class="w-64 shrink-0 border-r border-neutral-200 dark:border-neutral-800 flex flex-col"
>
  <div class="px-4 pt-4 pb-3">
    <div class="flex items-center justify-between">
      <h1 class="text-base font-semibold tracking-tight">Palamedes</h1>
      <button
        onclick={onOpenSettings}
        title="Settings"
        class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 text-base leading-none p-1 -mr-1 rounded hover:bg-neutral-100 dark:hover:bg-neutral-900"
        aria-label="Settings"
      >
        ⚙
      </button>
    </div>
    <button
      onclick={onNew}
      class="mt-3 w-full rounded-md bg-violet-500 hover:bg-violet-600 text-white px-3 py-1.5 text-sm font-medium"
    >
      + New chat
    </button>
  </div>

  <div class="flex-1 overflow-y-auto pb-4">
    {#if conversations.length === 0}
      <p class="px-4 pt-2 text-sm text-neutral-500">No conversations yet.</p>
    {/if}
    {#each grouped as group (group.bucket)}
      <div class="px-3 pt-3 pb-1 text-[11px] uppercase tracking-wider text-neutral-400 dark:text-neutral-500">
        {group.bucket}
      </div>
      <div class="px-2 space-y-0.5">
        {#each group.items as conv (conv.id)}
          <div
            class="group flex items-center justify-between gap-1 px-2 py-1.5 rounded-md cursor-pointer text-sm
                   {activeId === conv.id
                     ? 'bg-violet-100 dark:bg-violet-900/30 text-violet-900 dark:text-violet-100'
                     : 'hover:bg-neutral-100 dark:hover:bg-neutral-900 text-neutral-700 dark:text-neutral-300'}"
            onclick={() => onSelect(conv.id)}
            onkeydown={(e) => e.key === "Enter" && onSelect(conv.id)}
            role="button"
            tabindex="0"
          >
            <span class="truncate">{conv.title}</span>
            <button
              onclick={(e) => {
                e.stopPropagation();
                if (confirm(`Delete "${conv.title}"?`)) onDelete(conv.id);
              }}
              class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 text-neutral-500 hover:text-red-500 px-1 rounded"
              title="Delete"
              aria-label="Delete conversation"
            >
              ×
            </button>
          </div>
        {/each}
      </div>
    {/each}
  </div>
</aside>
