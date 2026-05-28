# Migrations

Numbered SQL migrations applied at startup by `src/migrations.rs`.

## Convention

- Filename: `NNNN_short_snake_case_name.sql` (four-digit zero-padded version
  number; non-overlapping per branch).
- Version `0001` is the **baseline** — represented implicitly by `schema.sql`,
  bootstrapped on first run after the migrator was introduced. Do not create
  `0001_*.sql` here.
- New migrations start at `0002` and increase monotonically.
- A migration is a plain `.sql` file with one or more statements; wrap
  multi-statement work in an explicit `BEGIN; ... COMMIT;` only if you need
  finer-grained transaction control than the migrator's per-file transaction.
- Migrations are append-only. Never edit a migration that has shipped — write
  a new one that fixes it.
- The runner records each applied version in `palamedes_schema_version`. A
  baseline-only DB ends up with a single row `(1, 'baseline', …)`.

## How the runner picks them up

`src/migrations.rs` declares a `MIGRATIONS` slice using `include_str!` so each
migration is embedded at compile time. **Add the new file here too** when you
create it — the directory itself is not scanned at runtime.

## Anti-patterns

- Don't `DROP` user data without an explicit acknowledgement path in the app.
- Don't depend on the existence of `palamedes_schema_version` inside a
  migration file — the runner inserts the row only after the SQL succeeds.
- Don't include `IF NOT EXISTS` on every statement defensively; the runner
  guarantees a migration runs at most once per DB.
