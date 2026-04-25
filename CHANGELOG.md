# Changelog

All notable changes to `stargaze` are documented here.

## [0.3.0] - Unreleased

### Added
- Command handlers moved from binary to library for improved test coverage
- Comprehensive test suite: 102 tests total (was 66), now with CLI integration tests
- `coverage_tests` module with 14 unit tests for new functionality:
  - `cosine_similarity()` tests (6 tests)
  - `calculate_semantic_boost()` tests (4 tests)
  - `truncate()` tests (4 tests)
  - `IndexedRepo::score()` tests (5 tests)
  - `indexed_repo_matches*()` tests (5 tests)
- `regenerate_embeddings()` CLI command and library function
- `Cmd::Embed` CLI variant (`stargaze embed`) to regenerate embeddings for existing repos
- Command handler tests (7 tests): `cmd_search_semantic`, `cmd_search_with_semantic`, `cmd_show_existing`, `cmd_show_missing`, `cmd_stats`, `cmd_list`
- `tests/cli.rs`: 8 CLI integration tests for end-to-end command verification
- Test coverage improved from ~56.85% to 68.19% (measured via tarpaulin)
- Embedding generation added to sync flow — all newly synced repos get embeddings automatically
- `calculate_semantic_boost()` free function for keyword-based semantic scoring fallback
- `IndexedRepo::score()` method for unified scoring across search modes
- `IndexedRepo::matches_*()` methods for structured query matching
- Test helper `make_repo()` for consistent test fixture creation
- `IndexedRepo::iter()` and `IndexedRepo::len()` / `is_empty()` convenience methods

### Changed
- Command handlers refactored: moved from `src/main.rs` to `src/lib.rs` for better testability
- `RepoIndex` now has `repos`, `cache` fields (was `repos`, `indexed_repos`, `cache`)
- `IndexedRepo::score()` takes only query string (no `semantic` flag) — semantic mode handled at `RepoIndex::search()` level
- `RepoIndex::search()` signature: now takes `semantic`, `fuzzy`, `case_sensitive` boolean flags
- `IndexedRepo::matches_*()` methods return `f64` score instead of `bool` for ranking

### Fixed
- Type annotations on all `async_std::task::spawn_blocking` results in command handlers
- `Cmd::Serve` syntax error (brace mismatch) in `src/main.rs`
- `Cmd::Stats` count type conversion (`usize` → `u64`)
- CLI test binary path resolution in `tests/cli.rs`

### Dependencies
- `fastembed = "4"` — already present in v0.2.0
- `once_cell = "1"` — already present in v0.2.0

## [0.2.0] - 2024-04-24

### Added
- **Semantic search with local embeddings** via `fastembed` (AllMiniLML6V2 model)
  - New `--semantic` flag for `stargaze search`
  - Vector embeddings generated and stored in `Repo.embedding` field
  - Cosine similarity scoring when embeddings available
  - Falls back to keyword-based boost when embeddings not available
  - Embeddings auto-generated during `stargaze sync`
- New `Repo.embedding: Option<Vec<f32>>` field (384-dim vectors)
- `generate_embedding()` and `cosine_similarity()` functions
- `LAZY_MODEL` static lazy-initialized fastembed model

### Changed
- `search()` now accepts `semantic: bool` parameter
- All `search()` callers updated (lib, main, tests, benches)
- `upsert_repos()` now generates embeddings for new repos

### Dependencies
- Added `fastembed = "4"`
- Added `once_cell = "1"`

## [0.1.2] - 2024-03-15

### Added
- MCP stdio server (`stargaze serve`) with tools: `search_stars`, `show_star`, `list_stars`, `stats`
- HTTP JSON API server (`stargaze api`) with Bearer token auth option
- README fetching during sync (`--with-readmes`)
- Language and topic filters for search
- LRU cache for search results
- `stargaze stats` and `stargaze list` commands
- `stargaze show <owner/name>` command
- Rayon parallel search
- Criterion benchmarks (7 groups)
- 66 tests across unit, integration, and live tiers

### Changed
- Redb 2.2 for embedded storage
- Weighted scoring: full_name (+3), description (+2), topics (+2), language (+1), readme (+1)

## [0.1.1] - 2024-02-01

### Added
- Initial release
- `stargaze sync` to cache GitHub stars
- Substring search across name, description, topics, language
- `--prune` flag to remove unstarred repos
- Systemd/Launchd scheduled sync examples
- GitHub Actions CI workflow
- Pure Rust, no C dependencies
