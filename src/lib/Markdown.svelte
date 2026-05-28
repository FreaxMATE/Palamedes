<script lang="ts">
  import { marked } from "marked";
  import hljs from "highlight.js";
  import { onDestroy, untrack } from "svelte";

  interface Props {
    source: string;
    /** When true, re-parse is throttled — useful during streaming. */
    throttle?: boolean;
  }

  let { source, throttle = false }: Props = $props();

  marked.setOptions({ breaks: true, gfm: true });

  const renderer = new marked.Renderer();
  renderer.code = ({ text, lang }) => {
    const language = lang && hljs.getLanguage(lang) ? lang : "plaintext";
    const highlighted = hljs.highlight(text, { language }).value;
    const encoded = encodeURIComponent(text);
    return (
      `<div class="code-block relative group">` +
      `<pre class="rounded-md overflow-x-auto my-2 p-3 pr-12 bg-neutral-900 text-sm">` +
      `<code class="hljs language-${language}">${highlighted}</code>` +
      `</pre>` +
      `<button type="button" data-copy-code="${encoded}" ` +
      `class="absolute top-2 right-2 opacity-0 group-hover:opacity-100 transition-opacity duration-150 ` +
      `rounded bg-neutral-800/80 hover:bg-neutral-700 text-neutral-200 text-[10px] px-2 py-0.5 uppercase tracking-wide">` +
      `copy</button>` +
      `</div>`
    );
  };

  function parse(s: string): string {
    return marked.parse(s, { renderer }) as string;
  }

  // The initial parse must read `source` exactly once at construction —
  // subsequent updates flow through the `$effect` below, which handles the
  // throttled case. `untrack` makes that one-shot intent explicit and
  // silences svelte-check's `state_referenced_locally` warning.
  let html = $state(untrack(() => parse(source)));
  let lastParse = 0;
  let pending: number | null = null;

  $effect(() => {
    const current = source;
    if (!throttle) {
      html = parse(current);
      return;
    }
    const now = performance.now();
    const elapsed = now - lastParse;
    const MIN_INTERVAL = 60;
    if (elapsed >= MIN_INTERVAL) {
      html = parse(current);
      lastParse = now;
    } else if (pending === null) {
      pending = window.setTimeout(() => {
        pending = null;
        html = parse(source);
        lastParse = performance.now();
      }, MIN_INTERVAL - elapsed);
    }
  });

  onDestroy(() => {
    if (pending !== null) window.clearTimeout(pending);
  });

  async function onClick(e: MouseEvent) {
    const target = (e.target as HTMLElement).closest(
      "[data-copy-code]",
    ) as HTMLElement | null;
    if (!target) return;
    const raw = target.getAttribute("data-copy-code") ?? "";
    try {
      await navigator.clipboard.writeText(decodeURIComponent(raw));
      const original = target.textContent;
      target.textContent = "copied";
      setTimeout(() => {
        if (target.textContent === "copied") target.textContent = original ?? "copy";
      }, 1200);
    } catch {
      /* ignore */
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events -->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div class="prose-chat" onclick={onClick}>
  {@html html}
</div>
