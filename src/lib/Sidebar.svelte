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
</script>

<aside
  class="w-64 shrink-0 border-r border-neutral-200 dark:border-neutral-800 flex flex-col"
>
  <div class="p-4 border-b border-neutral-200 dark:border-neutral-800">
    <div class="flex items-center justify-between">
      <h1 class="text-lg font-semibold">Palamedes</h1>
      <button
        onclick={onOpenSettings}
        title="Settings"
        class="text-neutral-500 hover:text-neutral-900 dark:hover:text-neutral-100 text-sm"
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

  <div class="flex-1 overflow-y-auto">
    {#if conversations.length === 0}
      <p class="p-4 text-sm text-neutral-500">No conversations yet.</p>
    {/if}
    {#each conversations as conv (conv.id)}
      <div
        class="group flex items-center justify-between px-3 py-2 cursor-pointer text-sm
               {activeId === conv.id
                 ? 'bg-neutral-100 dark:bg-neutral-800'
                 : 'hover:bg-neutral-50 dark:hover:bg-neutral-900'}"
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
          class="opacity-0 group-hover:opacity-100 text-neutral-500 hover:text-red-500 px-1"
          title="Delete"
          aria-label="Delete conversation"
        >
          ×
        </button>
      </div>
    {/each}
  </div>
</aside>
