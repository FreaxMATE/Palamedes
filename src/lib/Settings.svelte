<script lang="ts">
  import { getSetting, setSetting } from "./chat";
  import { onMount } from "svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let systemPrompt = $state("");
  let loaded = $state(false);

  onMount(async () => {
    systemPrompt = (await getSetting("system_prompt")) ?? "";
    loaded = true;
  });

  async function save() {
    await setSetting("system_prompt", systemPrompt);
    onClose();
  }
</script>

<div
  class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
  onclick={onClose}
  onkeydown={(e) => e.key === "Escape" && onClose()}
  role="dialog"
  tabindex="-1"
>
  <div
    class="bg-white dark:bg-neutral-900 rounded-lg shadow-xl w-[560px] max-w-[90vw] p-6"
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
    role="document"
  >
    <h2 class="text-lg font-semibold mb-4">Settings</h2>

    {#if loaded}
      <label class="block text-sm font-medium mb-1" for="sys-prompt">
        System prompt
      </label>
      <textarea
        id="sys-prompt"
        bind:value={systemPrompt}
        rows="8"
        class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-violet-500"
      ></textarea>
    {:else}
      <p class="text-sm text-neutral-500">Loading…</p>
    {/if}

    <div class="flex justify-end gap-2 mt-4">
      <button
        onclick={onClose}
        class="px-3 py-1.5 text-sm rounded-md hover:bg-neutral-100 dark:hover:bg-neutral-800"
      >
        Cancel
      </button>
      <button
        onclick={save}
        disabled={!loaded}
        class="px-3 py-1.5 text-sm rounded-md bg-violet-500 hover:bg-violet-600 text-white disabled:opacity-50"
      >
        Save
      </button>
    </div>
  </div>
</div>
