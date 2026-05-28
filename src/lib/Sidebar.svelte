<script lang="ts">
  import type { Conversation } from "./chat";
  import Icon from "./Icon.svelte";

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
  class="w-64 shrink-0 pal-bg-sunken border-r pal-border flex flex-col"
>
  <div class="px-4 pt-4 pb-3">
    <div class="flex items-center justify-between">
      <h1 class="wordmark">Palamedes</h1>
      <button
        onclick={onOpenSettings}
        title="Settings"
        class="pal-dim hover:opacity-100 opacity-70 leading-none p-1 -mr-1 rounded inline-flex items-center"
        aria-label="Settings"
      >
        <Icon name="settings" size={16} label="settings" />
      </button>
    </div>
    <button
      onclick={onNew}
      class="mt-3 w-full pal-accent-bg hover:opacity-90 text-white px-3 py-2 text-sm font-medium pal-shadow inline-flex items-center justify-center gap-1.5"
      style="border-radius: var(--pal-radius);"
    >
      <Icon name="plus" size={14} label="new" />
      <span>New chat</span>
    </button>
  </div>

  <div class="flex-1 overflow-y-auto pb-4">
    {#if conversations.length === 0}
      <p class="px-4 pt-2 text-sm pal-dim">No conversations yet.</p>
    {/if}
    {#each grouped as group (group.bucket)}
      <div class="px-3 pt-3 pb-1 text-[11px] uppercase tracking-wider pal-dim opacity-70">
        {group.bucket}
      </div>
      <div class="px-2 space-y-0.5">
        {#each group.items as conv (conv.id)}
          <div
            class="group flex items-center justify-between gap-1 px-2 py-1.5 cursor-pointer text-sm
                   {activeId === conv.id
                     ? 'pal-accent-soft-bg pal-accent-text'
                     : 'hover:opacity-100 opacity-90 pal-dim'}"
            style="border-radius: var(--pal-radius);"
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
              class="opacity-0 group-hover:opacity-100 transition-opacity duration-150 pal-dim hover:text-red-500 px-1 rounded inline-flex items-center"
              title="Delete"
              aria-label="Delete conversation"
            >
              <Icon name="close" size={12} label="delete" />
            </button>
          </div>
        {/each}
      </div>
    {/each}
  </div>
</aside>
