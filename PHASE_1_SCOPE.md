# Phase 1 Scope

**In scope:** Belief Ledger schema, Rust types, round-trip tests. That's it.

## Will NOT build in Phase 1

Even if it feels tempting. Re-read this before adding anything.

- No Svelte UI changes — the audit view is Phase 3
- No LLM calls for extraction — Phase 2
- No embeddings / sqlite-vec — Phase 5
- No provider abstraction — Phase 2
- No summarization — Phase 4
- No retrieval logic — Phase 5
- No receipts — Phase 5
- No capture inbox — Phase 6
- No daily recap — Phase 6
- No MCP
- No canvas view
- No tasks, calendar, mobile, teams, local inference

**If you catch yourself writing any of the above in Phase 1, stop and commit
what you have.** The foundation is the only thing that matters right now.
