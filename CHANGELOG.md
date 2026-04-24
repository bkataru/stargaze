# Changelog

All notable changes to `stargaze` are documented here.

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
