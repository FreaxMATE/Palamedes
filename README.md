# Palamedes

**The AI that remembers what you've ruled out.**

Audit-grade, local-first AI memory: see what your AI believes about you, see
*why* it believes it, and correct it when it's wrong — so its advice stays
grounded in your real situation.

## Why

Good advice needs your situation. Your AI keeps forgetting it — or remembering
it wrong, in a black box you can't inspect. You re-explain yourself every
session, you can't tell what it's sure of versus guessing, and when it's wrong
your only option is "delete everything and start over." And the more personal
the AI gets, the more it tends to just agree with you (personalization is the
single biggest driver of AI agreeableness — MIT, 2026).

Palamedes makes the AI's beliefs about you **first-class objects** you own:

- **Structural confidence** — how consistently a belief recurs across your
  conversations (never the model's self-rating, which is badly miscalibrated),
  shown as a coarse bucket, not a false-precise number.
- **Asserted vs. inferred** — "asserted" requires a verbatim quote from your own
  words, verified at extraction; everything else is inferred or hypothesized.
- **Provenance** — click any belief to the exact message it came from.
- **Version history** — every correction is preserved, not overwritten.
- **Ruled-out** — pin something wrong and it won't be re-inferred.

Corrections change the next answer, and travel to your other AI tools over MCP.

## What's different

| | ChatGPT / Claude memory | Mem0 / OpenMemory | Palamedes |
|---|:--:|:--:|:--:|
| See every belief | summary only | plain text | yes |
| How sure it is (per belief) | – | – | yes |
| Asserted vs. inferred | – | – | yes |
| Why it believes it (source) | – | – | yes, click to source |
| Correct it — and it stays corrected | delete only | – | yes, governed |
| "Stop assuming X" (ruled-out) | – | – | yes |
| On your hard drive | – | self-host | yes |
| Portable across AI tools (MCP) | – | yes | yes |
| Published open schema | – | – | yes |

## Who it's for

People who use AI on decisions that compound: founders weighing pivots, hiring,
and where to focus; builders evaluating ideas, markets, and which company to
join or start; researchers and analysts tracking what they've explored versus
ruled out.

## The Belief Schema

The shape of an auditable, correctable memory — `{ statement, confidence,
status, trust_class, version, provenance[] }` — is published as the **Belief
Schema for MCP** ([docs/BELIEF_SCHEMA_MCP.md](docs/BELIEF_SCHEMA_MCP.md),
CC-BY-SA-4.0). Palamedes is the reference implementation. Fork it.

## Known limitations

Audit-grade isn't a finished state — it's a thing you can verify, including
verifying what *isn't* yet covered. The honest list:

- **MCP consent is bucket-level today.** Granting a client read access lets
  it call every read tool; a per-category ACL (e.g. "preferences but not
  personal") is on the week-4 roadmap. A per-client 1000-reads/24h rate limit
  plus a full audit log of every read are already in place; you can see
  exactly what each connected AI has fetched.
- **Embedding-model swaps require a re-embed pass.** If you change the
  embedding model, Palamedes detects it at startup, archives the old vectors
  to `vec_beliefs_legacy_<dim>` so you can roll back, and shows a banner. The
  background loop refills `vec_beliefs` at the new dim — semantic search
  degrades to empty until that completes.
- **The UMAP memory map runs client-side.** Fine up to roughly 10k beliefs;
  past that, the initial render gets sluggish. Worker-side projection and
  delta sync are on the week-5 roadmap.
- **Extended graph edges (kNN, co-recall) are recomputed on every map open.**
  The composite index added in week 1 makes this cheap up to ~50k receipts;
  full materialization lands in week 2.
- **Confidence is recomputed on every fetch.** A single belief's confidence
  may change between two reads as reinforcement or recency drift — by
  design (it tracks the system's actual current state) but worth knowing.
- **No JSON export yet.** Your ledger lives in SQLite at the OS app-data
  path; for now you can `sqlite3` it directly. A versioned JSON export +
  re-import lands in week 2.
- **Sycophancy is structurally resisted, not actively detected.** The
  ledger makes it harder for the AI to flip on a single agreeable response,
  but there's no per-turn "this answer is over-affirming" signal yet — that
  research-heavy work is post-launch.

If you find a limitation we haven't surfaced, file an issue.

## Stack

- **Shell**: Tauri 2 · **Frontend**: Svelte 5 + Vite + Tailwind
- **Backend**: Rust (in-process via Tauri commands) · **DB**: SQLite (`rusqlite`)
  + `sqlite-vec`
- **LLM**: provider-agnostic (Nebius Token Factory / Kimi K2.5 today)
- **MCP**: in-process server exposing the audited corpus to Claude Desktop,
  Cursor, etc.

## Dev setup (NixOS)

```fish
direnv allow                     # or: nix develop
cp .env.example .env             # then paste your Nebius API key
pnpm install
cargo tauri dev
```

Local-first and open source. Your data stays on your machine unless you connect
it. License: AGPL-3.0.
