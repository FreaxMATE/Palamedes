<script lang="ts">
  import { getSetting, setSetting } from "./chat";
  import { ensureModels, getCachedModels } from "./modelStore";
  import { onMount } from "svelte";

  interface Props {
    onClose: () => void;
  }

  let { onClose }: Props = $props();

  let systemPrompt = $state("");
  let model = $state("");
  let models: string[] = $state([]);
  let modelsError: string | null = $state(null);
  let loaded = $state(false);

  onMount(async () => {
    // Use cached model list if available — opens instantly.
    const cached = getCachedModels();
    if (cached.models) {
      models = cached.models;
      modelsError = cached.error;
    }
    const [sp, m] = await Promise.all([
      getSetting("system_prompt"),
      getSetting("model"),
    ]);
    systemPrompt = sp ?? "";
    model = m ?? "moonshotai/Kimi-K2.5";
    loaded = true;

    // Kick off a refresh in the background (cheap if already cached).
    if (!cached.models) {
      try {
        models = await ensureModels();
      } catch (e: any) {
        modelsError = e?.message ?? String(e);
      }
    }
  });

  async function save() {
    await Promise.all([
      setSetting("system_prompt", systemPrompt),
      setSetting("model", model),
    ]);
    onClose();
  }
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div
  class="fixed inset-0 bg-black/50 flex items-center justify-center z-50"
  onclick={onClose}
  onkeydown={(e) => e.key === "Escape" && onClose()}
  role="dialog"
  tabindex="-1"
>
  <div
    class="bg-white dark:bg-neutral-900 rounded-lg shadow-xl w-[600px] max-w-[90vw] p-6 max-h-[80vh] overflow-y-auto"
    onclick={(e) => e.stopPropagation()}
    onkeydown={(e) => e.stopPropagation()}
    role="document"
  >
    <h2 class="text-lg font-semibold mb-4">Settings</h2>

    {#if loaded}
      <div class="space-y-5">
        <div>
          <label class="block text-sm font-medium mb-1" for="model-select">
            Model
          </label>
          {#if modelsError}
            <input
              id="model-select"
              type="text"
              bind:value={model}
              class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-violet-500"
            />
            <p class="text-xs text-red-500 mt-1">
              Couldn’t load model list: {modelsError}
            </p>
          {:else}
            <select
              id="model-select"
              bind:value={model}
              class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-violet-500"
            >
              {#each models as m (m)}
                <option value={m}>{m}</option>
              {/each}
            </select>
          {/if}
          <p class="text-xs text-neutral-500 mt-1">
            Used for new messages. Existing ones keep the model they were
            generated with.
          </p>
        </div>

        <div>
          <label class="block text-sm font-medium mb-1" for="sys-prompt">
            System prompt
          </label>
          <textarea
            id="sys-prompt"
            bind:value={systemPrompt}
            rows="8"
            class="w-full rounded-md border border-neutral-300 dark:border-neutral-700 bg-white dark:bg-neutral-950 px-3 py-2 text-sm focus:outline-none focus:ring-2 focus:ring-violet-500"
          ></textarea>
        </div>
      </div>
    {:else}
      <p class="text-sm text-neutral-500">Loading…</p>
    {/if}

    <div class="flex justify-end gap-2 mt-6">
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
