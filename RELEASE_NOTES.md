# Release pipeline — known issues to fix before v0.1.0

Findings from the **v0.0.1-rc.1** test tag (2026-05-30). Tag and the
draft release were deleted; main was reverted to `0.1.0`. These need
to be addressed before the real `v0.1.0` tag goes out.

## 1. Windows MSI rejects non-numeric pre-release identifiers

Error from `tauri build` on `windows-latest`:

```
failed to bundle project `optional pre-release identifier in app version
must be numeric-only and cannot be greater than 65535 for msi target`
```

The MSI bundler enforces a stricter version grammar than semver:
`MAJOR.MINOR.PATCH[-NUMERIC]`. `0.0.1-rc.1` fails because `rc.1` isn't
numeric.

**Fix options:**
- Use numeric-only pre-release IDs: `0.1.0-1`, `0.1.0-2`, …
- Or skip pre-release entirely: `0.1.0` → `0.1.1` → `0.2.0`. The
  `releaseDraft: true` flag in `.github/workflows/release.yml` already
  keeps every release as an unpublished draft, so a "v0.1.0" tag isn't
  publicly visible until we hit Publish in the GitHub UI.

Recommended: drop the `-rc.N` convention. Use clean semver
(`0.1.0`, `0.1.1`, …), rely on the draft flag for pre-launch
visibility.

## 2. macOS universal build fails on helper binaries

Error:

```
failed to bundle project Failed to copy binary from
"target/universal-apple-darwin/release/palamedes-mcp":
"…/release/palamedes-mcp" does not exist
```

The crate declares three binaries (`src-tauri/Cargo.toml`):

- `palamedes` (the Tauri app, default-run)
- `palamedes-mcp` (stdio MCP shim for Claude Desktop)
- `palamedes-mcp-serve` (headless MCP HTTP server)

Tauri's macOS bundler discovers every `[[bin]]` entry in `Cargo.toml`
and tries to copy each into the `.app`. For `universal-apple-darwin`
it expects each to exist as a `lipo`'d artifact at
`target/universal-apple-darwin/release/<name>` — but Tauri only does
the per-arch build + lipo for the **default-run** binary, so the
helpers are missing.

Ubuntu builds clean because the `.deb` / AppImage bundlers don't
iterate over all `[[bin]]` entries — they only package the main binary.

**Fix options:**

1. **Move helper bins to a separate workspace member.** Cleanest
   long-term. The helpers no longer appear in the palamedes-app
   crate, so Tauri's bundler doesn't see them. Workspace-level
   `cargo build --bins` can still produce them when needed.

2. **Drop the universal target.** Use two matrix entries:
   - `macos-latest` (Apple Silicon, arm64) — `--target aarch64-apple-darwin`
   - `macos-13` (Intel) — `--target x86_64-apple-darwin`

   Two .dmg / .app artifacts instead of one universal bundle.
   No lipo step. Simpler, still covers both architectures.

3. **Set `bundle.macOS.binaries` explicitly** to `["palamedes"]` so
   Tauri stops auto-discovering helpers. Smallest diff, but Tauri
   docs are thin on whether this config path actually exists in 2.x
   — verify before relying on it.

Recommended: option 2 (separate per-arch matrix) for the v0.1.0 tag;
option 1 (workspace split) as a follow-up cleanup once we have
breathing room.

## 3. Ubuntu build worked

Worth saying out loud: the Linux build succeeded end-to-end on
`ubuntu-22.04`. The `glibc` baseline is broad enough that the resulting
AppImage / `.deb` should run on any reasonably-recent Linux. That
shipping path is already functional.

## What this confirmed

The release pipeline is **not** ready for the v0.1.0 tag as-is. But
the failure was a known-unknowns hit, not a structural problem:

- Linux is fine.
- Windows needs a version-format change.
- macOS needs a Cargo workspace tweak or a matrix change.

Both fixes are small. Cost to ship a working v0.1.0: ~half a day.
