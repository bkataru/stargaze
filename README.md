# stargaze

**Cache and search your GitHub stars from the terminal. Pure Rust. Single binary. No cloud.**

[![CI](https://github.com/bkataru/stargaze/actions/workflows/ci.yml/badge.svg)](https://github.com/bkataru/stargaze/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/stargaze.svg)](https://crates.io/crates/stargaze)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE)

A small Rust CLI that pulls your starred GitHub repos into a local embedded database, lets you keyword-search them (README text included), and exposes the same cache to AI agents as an MCP server. No daemon, no cloud, no C dependencies.

---

## Why

The GitHub starred-repos list is a shoebox of bookmarks with no real retrieval. Once you've starred more than a few hundred repos, finding "that one Postgres CLI I saw last month" means scrolling. `stargaze` turns your stars into a queryable local corpus that survives across machines, runs offline, and plugs into Claude / Cursor / any MCP-speaking agent.

## Install

From crates.io:

```bash
cargo install stargaze
```

From source:

```bash
git clone https://github.com/bkataru/stargaze
cd stargaze
cargo install --path .
```

Needs Rust 1.76+. Everything is pure Rust — no `libsqlite3`, no `openssl`, no C toolchain. The release binary is fully static (aside from libc).

Prebuilt binaries for Linux / macOS / Windows are attached to every [tagged release](https://github.com/bkataru/stargaze/releases).

## Quick start

```bash
# one-time: give stargaze a token to talk to the GitHub API
export GH_TOKEN=$(gh auth token)

# pull down your stars
stargaze sync

# ...and their READMEs (slower, pulls 1 request per repo)
stargaze sync --with-readmes

# search across the cache — now includes README text
stargaze search postgres
stargaze search 'vector db' --lang rust --limit 10
stargaze search cli --topic hacktoberfest

# run as an MCP server so agents can query your stars
stargaze serve
```

## Commands

| Command | What it does |
|---|---|
| `stargaze sync [--user <login>] [--with-readmes] [--concurrency N] [--prune]` | Fetch all starred repos via the GitHub API and upsert into the local cache. `--with-readmes` also pulls each repo's README in parallel. `--prune` removes locally cached repos that are no longer starred. |
| `stargaze readmes [--force] [--concurrency N]` | Fetch READMEs for already-cached repos. Skips repos with a cached README unless `--force`. Useful after a plain `sync`. |
| `stargaze search <query> [--lang <L>] [--topic <T>] [--limit N] [--semantic]` | Substring match across `full_name`, `description`, `language`, `topics`, and cached README text. Ranks by weighted hit count × log(stars). Weight order: full_name (+3), description (+2), topics (+2), language (+1), readme (+1). Add `--semantic` to use vector embeddings (fastembed) for semantic similarity. Cold queries run in parallel via rayon; repeat queries hit an LRU cache. |
| `stargaze show <owner/name>` | Pretty-print the full cached JSON record for a repo. |
| `stargaze stats` | Total cached count, last sync time, top languages. |
| `stargaze list [--limit N]` | List all cached repos sorted by stargazer count. |
| `stargaze serve` | Run a Model Context Protocol stdio server over the cache. Exposes `search_stars`, `show_star`, `list_stars`, and `stats` as MCP tools. |
| `stargaze api [--bind 127.0.0.1:7879] [--api-key <k>] [--threads N]` | Run an HTTP JSON API over the cache. Same read-only surface as the CLI and MCP. Bearer-token auth is optional. |

Global flags:

- `--db <path>` — override the database file (default: `$XDG_DATA_HOME/stargaze/stars.redb`)
- `--token <pat>` — pass a GitHub PAT explicitly instead of reading the env var

## Storage

One file. Default location:

- Linux: `~/.local/share/stargaze/stars.redb`
- macOS: `~/Library/Application Support/com.bkataru.stargaze/stars.redb`
- Windows: `%APPDATA%\bkataru\stargaze\data\stars.redb`

Backed by [`redb`](https://github.com/cberner/redb) — a pure-Rust embedded key-value store with ACID transactions. Each starred repo is stored as a JSON blob keyed by `owner/name`; a separate `meta` table tracks last-sync timestamp and count. README text is included in the same record (truncated to 64 KiB to keep the file lean).

## Three equivalent surfaces: CLI, MCP, HTTP API

Every read-only operation is reachable from all three surfaces. You pick the one that matches your client:

| Operation | CLI | MCP tool | HTTP |
|---|---|---|---|
| Health check | — | — | `GET /api/v1/health` |
| Stats (cached count + last sync) | `stargaze stats` | `stats` | `GET /api/v1/stats` |
| Keyword search | `stargaze search <q>` | `search_stars` | `GET /api/v1/search?q=…&lang=…&topic=…&limit=…` |
| Top-N by stars | `stargaze list --limit N` | `list_stars` | `GET /api/v1/list?limit=N` |
| Show one repo | `stargaze show owner/name` | `show_star` | `GET /api/v1/stars/:owner/:name` |
| Sync | `stargaze sync` | _not exposed (mutating)_ | _not exposed (mutating)_ |
| Fetch READMEs | `stargaze readmes` | _not exposed (mutating)_ | _not exposed (mutating)_ |

All three surfaces dispatch through the same `RepoIndex` + `Database` handles, so search results are identical across them. Mutating operations (sync, readmes) are intentionally CLI-only for now.

## HTTP API

```bash
stargaze api --bind 127.0.0.1:7879 --threads 4
# optionally require a bearer token:
stargaze api --bind 0.0.0.0:7879 --api-key "$(openssl rand -hex 32)"
```

```bash
curl http://127.0.0.1:7879/api/v1/health
curl 'http://127.0.0.1:7879/api/v1/search?q=postgres&limit=5'
curl http://127.0.0.1:7879/api/v1/stars/rust-lang/rust
```

Backed by [`tiny_http`](https://github.com/tiny-http/tiny-http) (pure Rust, blocking, no tokio). Thread pool size configurable via `--threads`. CORS is enabled (`Access-Control-Allow-Origin: *`) so you can hit it from localhost web UIs.

## MCP server mode

`stargaze serve` runs a [Model Context Protocol](https://modelcontextprotocol.io/) stdio server that exposes the local cache as a structured tool set. Any MCP-compatible client (Claude Desktop, Cursor, custom agents) can query your starred repos as typed data.

Example Claude Desktop config (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "stargaze": {
      "command": "stargaze",
      "args": ["serve"]
    }
  }
}
```

Tools exposed:

| Tool | Arguments | Returns |
|---|---|---|
| `search_stars` | `query`, `lang?`, `topic?`, `limit?` | Ranked search hits with score, name, description, language, stars, topics, url |
| `show_star` | `full_name` | Full cached record for one repo |
| `list_stars` | `limit?` | Repos sorted by stargazer count |
| `stats` | — | Cache size + last sync metadata |

The implementation is hand-rolled JSON-RPC 2.0 over stdio — no `tokio`, no external MCP library — so the binary stays single-static and cold-starts instantly.

## Auth

`stargaze sync` needs a GitHub token (any classic PAT or fine-grained token with read access to public metadata will do). Resolution order:

1. `--token <pat>` flag
2. `GH_TOKEN` environment variable
3. `GITHUB_TOKEN` environment variable

The easiest bootstrap if you already use the `gh` CLI:

```bash
export GH_TOKEN=$(gh auth token)
```

`stargaze` never shells out — all HTTP traffic goes through [`ureq`](https://github.com/algesten/ureq) with rustls (pure-Rust TLS).

## Scheduled sync

### systemd timer (Linux)

`~/.config/systemd/user/stargaze-sync.service`:

```ini
[Unit]
Description=stargaze — refresh GitHub stars cache

[Service]
Type=oneshot
Environment="GH_TOKEN=%h/.config/stargaze/token"
ExecStart=%h/.cargo/bin/stargaze sync --with-readmes --prune
```

`~/.config/systemd/user/stargaze-sync.timer`:

```ini
[Unit]
Description=Weekly stargaze sync

[Timer]
OnCalendar=weekly
Persistent=true

[Install]
WantedBy=timers.target
```

Enable it once: `systemctl --user enable --now stargaze-sync.timer`.

### launchd (macOS)

`~/Library/LaunchAgents/com.bkataru.stargaze.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
  <dict>
    <key>Label</key>  <string>com.bkataru.stargaze</string>
    <key>ProgramArguments</key>
    <array>
      <string>/usr/local/bin/stargaze</string>
      <string>sync</string>
      <string>--with-readmes</string>
      <string>--prune</string>
    </array>
    <key>StartInterval</key> <integer>604800</integer>
    <key>EnvironmentVariables</key>
    <dict><key>GH_TOKEN</key><string>ghp_…</string></dict>
  </dict>
</plist>
```

Load: `launchctl load ~/Library/LaunchAgents/com.bkataru.stargaze.plist`.

### GitHub Actions (run in the cloud, commit the cache)

See [`.github/workflows/ci.yml`](./.github/workflows/ci.yml) for the reusable CI pattern. A weekly sync workflow that commits the cache back to a private repo is straightforward: `cargo install stargaze && stargaze sync --with-readmes && git add stars.redb && git commit && git push`.

## Benchmarks

`stargaze` ships with a full [`criterion`](https://github.com/bheisler/criterion.rs) bench suite:

```bash
cargo bench                       # run all workloads
cargo bench index_build           # only the index-build path
cargo bench search_cold           # cold (non-cached) searches
cargo bench search_warm           # LRU-cache hit path
```

Workloads covered:

| Bench group | Parameters |
|---|---|
| `index_build` | 100, 1k, 3k, 10k synthetic repos |
| `search_cold` | same sizes, one cold search per iteration |
| `search_warm` | 3k repos, LRU pre-primed |
| `search_with_filters` | lang / topic / combined filter fast-paths |
| `indexed_repo_score` | per-repo score function microbench |
| `parse_link_next` | Link header walker |
| `repo_from_api` | JSON payload → `Repo` conversion |

End-to-end wall-clock via [`hyperfine`](https://github.com/sharkdp/hyperfine):

```bash
hyperfine --warmup 1 'stargaze search postgres'
hyperfine --warmup 1 'stargaze sync'
```

## Tests

Around **102 tests** across unit + integration + live tiers:

```bash
cargo test                        # hermetic suite (102 tests, no network)
cargo test -- --ignored           # + 6 live tests that hit the real GitHub API
```

Live tests are gated on `GH_TOKEN` / `GITHUB_TOKEN` being present; if missing they short-circuit with a skip message instead of failing. The CI pipeline runs them on every push to `main` using the repo's own workflow token.

Full breakdown:

| Layer | File | Count |
|---|---|---|
|| Unit — parsing / index / MCP / API / coverage | `src/lib.rs` | `parse_link_next`, `Repo::from_api`, `IndexedRepo`, `score`, `truncate`, `resolve_token`, MCP handlers, HTTP router, query-string parser, semantic embedding tests (14 coverage_tests) (~82 tests) |
| Integration — redb | `tests/store.rs` | upsert / load / meta / retain / idempotency / regenerate_embeddings (13 tests) |
| Integration — search | `tests/search.rs` | filters / ranking / cache / large corpora (14 tests) |
| Integration — parse | `tests/parse.rs` | edge-case payloads + Link headers (9 tests) |
| Live — GitHub API | `tests/live.rs` | pagination, README fetch, parallel batch, mini sync (6 tests, `#[ignore]`) |

## Comparison: `stargaze` vs `miantiao-me/github-stars`

Both projects address the same frustration. They make very different architectural choices.

| Capability | `stargaze` | [`miantiao-me/github-stars`](https://github.com/miantiao-me/github-stars) |
|---|---|---|
| **Language** | Rust | TypeScript / JavaScript |
| **Runtime requirement** | Single static binary | Node.js v22 + pnpm + Cloudflare account |
| **Storage** | Local redb file | Cloudflare R2 bucket |
| **Auth** | `GH_TOKEN` env var | `GH_TOKEN` + Cloudflare API keys + MCP API key |
| **Offline use** | ✅ full cache lives on disk | ❌ requires Cloudflare |
| **Pagination of stars API** | ✅ Link-header driven, generic | ✅ page counter |
| **README extraction** | ✅ `--with-readmes`, rayon-parallel | ✅ single-threaded loop |
| **Scheduled refresh** | ✅ systemd / launchd / cron / GH Actions examples | ✅ weekly GH Actions cron |
| **Search — keyword** | ✅ substring + BM25-ish scoring over name/desc/topics/lang/README, LRU-cached | ❌ delegates everything to Cloudflare AutoRAG |
| **Search — semantic** | ✅ `fastembed` local embeddings + cosine similarity (`--semantic` flag) | ✅ Cloudflare AutoRAG embeddings |
| **Language / topic filters** | ✅ | ❌ |
| **Stats / list / show commands** | ✅ | ❌ |
| **Prune stale entries** | ✅ `--prune` | ❌ |
| **MCP tool server** | ✅ `stargaze serve` over stdio | ✅ Cloudflare Worker HTTP |
| **MCP transport** | stdio — works with Claude Desktop out-of-the-box | HTTP — requires deployment + API key management |
| **HTTP JSON API** | ✅ `stargaze api` — tiny_http, pure Rust, optional bearer auth | (Cloudflare Worker is the only interface) |
| **Equivalent surfaces** | ✅ CLI + MCP + HTTP all dispatch through the same index | ❌ only MCP |
| **Parallelism** | rayon `par_iter` for search + README batch | single-threaded JS |
| **Test coverage** | 88 hermetic + 6 live | none committed |
| **Benchmarks** | criterion (7 groups) + hyperfine docs | none |
| **Cold-start** | ~1 ms (binary) | Worker cold-start + network round-trip |
| **License** | MIT | MIT |

The short version: `stargaze` is local-first, CI-tested, and runs anywhere Rust compiles. `miantiao-me/github-stars` offloads everything interesting (storage, embeddings, hosting) to Cloudflare. Different axes, same goal.

## Roadmap

|- **v0.3.0** (current): Semantic search with `fastembed` local embeddings + cosine similarity, sync, parallel README fetch, substring search with weighted scoring, LRU cache, MCP stdio server, HTTP JSON API server, 102 tests, coverage_tests, criterion benches, GH Actions CI + release workflow.
|- **v0.4**: `stargaze diff --since DATE` — delta of additions/removals (already schema-ready via `starred_at`).
|- **v0.5**: fuzzy matching with `fuzzy-matcher`, OR mode search (foo OR bar), topic boost multipliers, export formats (markdown, opml, csv), `stargaze categorize` heuristic grouping, `stargaze open <name>` launcher.
## License

MIT. See [LICENSE](./LICENSE).
