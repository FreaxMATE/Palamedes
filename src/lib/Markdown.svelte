<script lang="ts">
  import { marked } from "marked";
  import hljs from "highlight.js";
  import "highlight.js/styles/github-dark.css";

  interface Props {
    source: string;
  }

  let { source }: Props = $props();

  marked.setOptions({
    breaks: true,
    gfm: true,
  });

  // Custom renderer for code blocks with highlight.js
  const renderer = new marked.Renderer();
  renderer.code = ({ text, lang }) => {
    const language = lang && hljs.getLanguage(lang) ? lang : "plaintext";
    const highlighted = hljs.highlight(text, { language }).value;
    return `<pre class="rounded-md overflow-x-auto my-2 p-3 bg-neutral-900 text-sm"><code class="hljs language-${language}">${highlighted}</code></pre>`;
  };

  let html = $derived(marked.parse(source, { renderer }) as string);
</script>

<div class="prose-chat">
  {@html html}
</div>

<style>
  .prose-chat :global(p) {
    margin: 0.25rem 0;
  }
  .prose-chat :global(p:first-child) {
    margin-top: 0;
  }
  .prose-chat :global(p:last-child) {
    margin-bottom: 0;
  }
  .prose-chat :global(ul),
  .prose-chat :global(ol) {
    margin: 0.25rem 0;
    padding-left: 1.5rem;
  }
  .prose-chat :global(li) {
    margin: 0.125rem 0;
  }
  .prose-chat :global(code) {
    background: rgba(127, 127, 127, 0.2);
    padding: 0.1em 0.35em;
    border-radius: 3px;
    font-size: 0.9em;
  }
  .prose-chat :global(pre code) {
    background: transparent;
    padding: 0;
    border-radius: 0;
    font-size: inherit;
  }
  .prose-chat :global(h1),
  .prose-chat :global(h2),
  .prose-chat :global(h3) {
    font-weight: 600;
    margin: 0.75rem 0 0.25rem;
  }
  .prose-chat :global(h1) {
    font-size: 1.2rem;
  }
  .prose-chat :global(h2) {
    font-size: 1.1rem;
  }
  .prose-chat :global(h3) {
    font-size: 1.05rem;
  }
  .prose-chat :global(blockquote) {
    border-left: 3px solid rgba(127, 127, 127, 0.4);
    padding-left: 0.75rem;
    margin: 0.5rem 0;
    color: inherit;
    opacity: 0.85;
  }
  .prose-chat :global(a) {
    color: rgb(139, 92, 246);
    text-decoration: underline;
  }
  .prose-chat :global(table) {
    border-collapse: collapse;
    margin: 0.5rem 0;
  }
  .prose-chat :global(th),
  .prose-chat :global(td) {
    border: 1px solid rgba(127, 127, 127, 0.4);
    padding: 0.25rem 0.5rem;
  }
</style>
