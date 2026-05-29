# Palamedes

**The AI that remembers what you've ruled out.**

Audit-grade, local-first AI memory: see what your AI believes about you, see
*why* it believes it, and correct it when it's wrong — so its advice stays
grounded in your real situation.

> **Status: pre-launch, dogfooding.** Public so the open Belief Schema MCP
> standard has a public reference implementation. Feature-complete enough to
> use day-to-day; the [Known limitations](#known-limitations) section is the
> live punch list. Formal launch a few weeks out.

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
status, trust_class, version, provenance[] }` — is the data primitive
Palamedes is built around. A standalone, language-neutral spec is in
preparation; for now the canonical reference is the Rust source under
`src-tauri/src/ledger.rs` and the SQLite schema in `src-tauri/schema.sql`.

## Audit chain

Every write to the belief ledger lands as a row in a tamper-evident
SHA-256 hash chain stored in a separate SQLite file (`audit.db` next to
`palamedes.db`). The chain is fully self-verifying — `audit_chain_verify`
walks it top-to-bottom and re-derives every hash; any in-place edit of
a past row falls out immediately.

On first launch Palamedes also generates a local Ed25519 keypair at
`<data_dir>/audit-key.{priv,pub}` (mode 0600). The `audit_chain_sign_head`
command signs the current head and writes `<data_dir>/audit-head.sig` —
that file is the portable attestation. Commit it to a public repo, email
it to yourself, or pin it anywhere outside the laptop, and you've
anchored the chain to a fixed external point. Export envelopes
(`export_ledger_signed`) carry the same signature, and
`import_ledger_verified` refuses any envelope whose signature doesn't
match the local pubkey.

Threat model: the keypair is local-only — a laptop attacker with full
disk access can both rewrite `audit.db` *and* re-sign with the key. The
value is portability: once the signed head leaves the laptop, it
becomes evidence a later audit can verify against.

## Known limitations

Audit-grade isn't a finished state — it's a thing you can verify, including
verifying what *isn't* yet covered. The honest list:

- **MCP consent is bucket-level today.** Granting a client read access lets
  it call every read tool; a per-category ACL (e.g. "preferences but not
  personal") is still on the roadmap. A per-client 1000-reads/24h rate
  limit plus a full audit log of every read are already in place; you can
  see exactly what each connected AI has fetched.
- **Embedding-model swaps require a re-embed pass.** If you change the
  embedding model, Palamedes detects it at startup, archives the old vectors
  to `vec_beliefs_legacy_<dim>` so you can roll back, and shows a banner. The
  background loop refills `vec_beliefs` at the new dim — semantic search
  degrades to empty until that completes.
- **The UMAP memory map runs client-side.** Fine up to roughly 10k beliefs;
  past that, the initial render gets sluggish. Worker-side projection and
  delta sync are still pending.
- **Confidence is recomputed on every fetch.** A single belief's confidence
  may change between two reads as reinforcement or recency drift — by
  design (it tracks the system's actual current state) but worth knowing.
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
