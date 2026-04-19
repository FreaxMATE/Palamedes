# Palamedes

Personal AI chat app with branching conversations and (later) long-term memory.

- **Shell**: Tauri 2
- **Frontend**: Svelte 5 + Vite + TailwindCSS
- **Backend**: Rust (in-process via Tauri commands)
- **DB**: SQLite (via `rusqlite`)
- **LLM**: Nebius Token Factory (Kimi K2 primary)

## Dev setup (NixOS)

```fish
direnv allow                     # or: nix develop
cp .env.example .env             # then paste your Nebius API key
pnpm install
cargo tauri dev
```

## Phase 1 scope

Linux desktop, local-only: streaming chat → persistence → branching.
Memory, mobile, remote deployment come in later phases.
