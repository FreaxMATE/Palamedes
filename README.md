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
