# AGENTS.md

Guidance for coding agents working on this repo.

## What this project is

`stargaze` is a personal CLI tool for bkataru to cache and search his GitHub stars. Small scope, sharp edges. Not a framework, not a platform.

## Hard rules

1. **Pure Rust only.** No C dependencies. No `rusqlite` (wraps libsqlite3), no `openssl-sys`, no `bindgen`-based crates. Use:
   - `redb` for storage (not `sqlx`, not `diesel`, not `sled` if it regresses).
   - `ureq` with `rustls` for HTTP (not `reqwest` unless rustls-only).
   - `tantivy` if we add FTS (pure Rust).
2. **No tokio / no async runtime.** This is a CLI that runs once and exits. Blocking HTTP + blocking storage is fine and avoids 200k LOC of dependencies.
3. **No shelling out.** Do not call `gh`, `git`, `curl`, or any external process. All work happens in Rust. The only env var we read is the GitHub token.
4. **Single static binary.** `cargo build --release` must produce a relocatable binary with no runtime dependencies beyond libc.
5. **Stay small.** v0 target: under 500 LOC. If you feel a temptation to add a crate, ask first — the existing dependency list is the budget.

## Layout

```
stargaze/
├── Cargo.toml
├── README.md
├── AGENTS.md         (this file)
├── CLAUDE.md         (pointer to this file)
├── LICENSE
└── src/
    └── main.rs       (single-file v0)
```

If v0 grows beyond ~600 LOC, split into `gh.rs`, `store.rs`, `search.rs`, `cli.rs` — but not before.

## Commands

```bash
cargo check                      # quick type-check
cargo build                      # debug build
cargo build --release            # release build (lto thin, strip)
cargo test                       # run unit tests
cargo run -- sync                # sync stars to local cache
cargo run -- search postgres     # query the cache
```

## Testing conventions

- Unit tests live at the bottom of `main.rs` under `#[cfg(test)] mod tests`.
- Test `parse_link_next`, `Repo::from_api` with representative payloads, `matches` case-insensitivity, and `score` ordering.
- Don't test against the live GitHub API in unit tests. Sync is tested manually via `cargo run -- sync` against the real service.
- When a bug is fixed, add a regression test that would have caught it.

## Style

- `rustfmt` default settings. Run `cargo fmt` before committing.
- `clippy::all` clean. Run `cargo clippy -- -D warnings` before committing.
- No `.unwrap()` in non-test code. Use `anyhow::Result` and `?`.
- Error messages should tell the user what to do next, not just what went wrong.

## What NOT to add

- Web UI
- Server mode
- Cloud sync
- Multi-user support
- Plugin system
- Config file (CLI flags + env vars are enough)
- Telemetry

This is a personal tool. Scope creep kills it.

## Model notes

- For refactors within a single file, Claude Sonnet 4.6 is fine.
- For architectural changes (splitting files, adding a search index, schema migration), use Claude Opus 4.6.
- When adding a new dependency, justify it in the commit message. "Because the README said so" is not a justification.
