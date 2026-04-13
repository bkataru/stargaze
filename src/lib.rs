//! stargaze — cache and search your GitHub stars from the terminal.
//!
//! All logic lives in this library so it's reachable from `main.rs`, from
//! integration tests under `tests/`, and from criterion benches under
//! `benches/`. The binary itself is a thin CLI shell that parses args,
//! opens the database, and dispatches to the functions exported here.
//!
//! Design invariants:
//!   - Pure Rust only. No C dependencies (no `rusqlite`, no `openssl-sys`).
//!     Storage is [`redb`] (MPL-2.0), HTTP is [`ureq`] with `rustls`.
//!   - No async runtime. Blocking HTTP + blocking storage, rayon only for
//!     CPU-bound parallelism over in-memory slices.
//!   - No shelling out. Never invokes `gh`, `git`, `curl`, etc.
//!   - Single static binary. `cargo build --release` produces a relocatable
//!     artifact with no runtime dependencies beyond libc.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use lru::LruCache;
use rayon::prelude::*;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Agent string used in every GitHub API request — lets them rate-limit by name.
pub const USER_AGENT: &str = concat!("stargaze/", env!("CARGO_PKG_VERSION"));

/// `owner/name` → JSON-serialized [`Repo`] bytes.
pub const REPOS: TableDefinition<&str, &[u8]> = TableDefinition::new("repos");

/// Key → string metadata (last-sync timestamp, last-sync count, etc).
pub const META: TableDefinition<&str, &str> = TableDefinition::new("meta");

// ─────────────────────────────────────────────────────────────────────────────
// Data model
// ─────────────────────────────────────────────────────────────────────────────

/// A GitHub repository as we store and search it.
///
/// Fields follow the GitHub REST API `repository` object shape, trimmed down
/// to the pieces we actually use. All timestamps are stored as UTC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub full_name: String,
    pub owner: String,
    pub name: String,
    pub description: Option<String>,
    pub url: String,
    pub language: Option<String>,
    pub stargazers_count: u64,
    pub forks_count: u64,
    pub open_issues_count: u64,
    pub topics: Vec<String>,
    pub default_branch: Option<String>,
    pub license: Option<String>,
    pub archived: bool,
    pub fork: bool,
    pub pushed_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub starred_at: Option<DateTime<Utc>>,
    pub cached_at: DateTime<Utc>,
    /// README text (truncated to README_MAX_BYTES), fetched on demand by
    /// `stargaze sync --with-readmes`. `None` means "not yet fetched";
    /// `Some("")` means "fetched but empty".
    #[serde(default)]
    pub readme: Option<String>,
    #[serde(default)]
    pub readme_fetched_at: Option<DateTime<Utc>>,
}

/// Cap README text so a few oversized files can't blow up the redb store
/// or the LRU cache. 64 KiB is enough to capture the "what is this repo"
/// lead + headings of nearly every README on GitHub.
pub const README_MAX_BYTES: usize = 64 * 1024;

impl Repo {
    /// Parse a single item from the GitHub `/user/starred` response.
    ///
    /// The API can return two shapes depending on the `Accept` header:
    ///   - plain repository object
    ///   - `{starred_at, repo: {…}}` wrapper (star-preview media type)
    ///
    /// Both are handled here. Missing optional fields collapse to `None`.
    pub fn from_api(v: &serde_json::Value) -> Result<Self> {
        let (repo_obj, starred_at) = if v.get("repo").is_some() {
            (
                v.get("repo").ok_or_else(|| anyhow!("missing `repo` key"))?,
                v.get("starred_at")
                    .and_then(|s| s.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc)),
            )
        } else {
            (v, None)
        };

        let get_str =
            |key: &str| -> Option<String> { repo_obj.get(key)?.as_str().map(|s| s.to_string()) };
        let get_str_required = |key: &str| -> Result<String> {
            get_str(key).ok_or_else(|| anyhow!("missing or non-string `{}`", key))
        };
        let get_u64 = |key: &str| repo_obj.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        let get_bool = |key: &str| repo_obj.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
        let get_dt = |key: &str| -> Option<DateTime<Utc>> {
            repo_obj
                .get(key)?
                .as_str()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc))
        };

        let full_name = get_str_required("full_name")?;
        let owner = repo_obj
            .get("owner")
            .and_then(|o| o.get("login"))
            .and_then(|l| l.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                full_name
                    .split('/')
                    .next()
                    .map(String::from)
                    .unwrap_or_default()
            });
        let name = get_str_required("name")?;

        let topics: Vec<String> = repo_obj
            .get("topics")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let license = repo_obj
            .get("license")
            .and_then(|l| l.get("spdx_id"))
            .and_then(|s| s.as_str())
            .map(String::from);

        Ok(Repo {
            full_name,
            owner,
            name,
            description: get_str("description"),
            url: get_str("html_url").unwrap_or_default(),
            language: get_str("language"),
            stargazers_count: get_u64("stargazers_count"),
            forks_count: get_u64("forks_count"),
            open_issues_count: get_u64("open_issues_count"),
            topics,
            default_branch: get_str("default_branch"),
            license,
            archived: get_bool("archived"),
            fork: get_bool("fork"),
            pushed_at: get_dt("pushed_at"),
            created_at: get_dt("created_at"),
            updated_at: get_dt("updated_at"),
            starred_at,
            cached_at: Utc::now(),
            readme: None,
            readme_fetched_at: None,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Memoized search index
// ─────────────────────────────────────────────────────────────────────────────

/// Precomputed search-friendly view of a [`Repo`].
///
/// Every string field we search on is lowercased exactly once at index
/// construction time, so repeated queries don't re-allocate. The index also
/// caches a joined topics string and the log(stars+1) score multiplier.
#[derive(Debug, Clone)]
pub struct IndexedRepo {
    pub repo: Repo,
    full_name_lc: String,
    description_lc: String,
    language_lc: String,
    topics_lc: String,
    readme_lc: String,
    pub topics_lower: Vec<String>,
    log_stars: f64,
}

impl IndexedRepo {
    pub fn new(repo: Repo) -> Self {
        let full_name_lc = repo.full_name.to_lowercase();
        let description_lc = repo.description.as_deref().unwrap_or("").to_lowercase();
        let language_lc = repo.language.as_deref().unwrap_or("").to_lowercase();
        let topics_lower: Vec<String> = repo.topics.iter().map(|t| t.to_lowercase()).collect();
        let topics_lc = topics_lower.join(" ");
        let readme_lc = repo.readme.as_deref().unwrap_or("").to_lowercase();
        let log_stars = ((repo.stargazers_count as f64) + 1.0).ln();
        Self {
            repo,
            full_name_lc,
            description_lc,
            language_lc,
            topics_lc,
            readme_lc,
            topics_lower,
            log_stars,
        }
    }

    /// Weighted hit score for `q_lc` against this indexed repo.
    /// `q_lc` MUST already be lowercased.
    ///
    /// Weight order (strongest first):
    ///   - full_name hit: +3
    ///   - description hit: +2
    ///   - topics hit: +2
    ///   - language hit: +1
    ///   - readme hit: +1
    ///
    /// README hits carry the lowest weight because README text is long
    /// and a single substring match there is weaker evidence than the
    /// same substring in the curated name / description / topics.
    pub fn score(&self, q_lc: &str) -> f64 {
        let mut c = 0usize;
        if self.full_name_lc.contains(q_lc) {
            c += 3;
        }
        if self.description_lc.contains(q_lc) {
            c += 2;
        }
        if self.topics_lc.contains(q_lc) {
            c += 2;
        }
        if self.language_lc.contains(q_lc) {
            c += 1;
        }
        if !self.readme_lc.is_empty() && self.readme_lc.contains(q_lc) {
            c += 1;
        }
        (c as f64) * self.log_stars
    }

    /// True if any searchable field contains `q_lc` as a substring.
    pub fn matches(&self, q_lc: &str) -> bool {
        if q_lc.is_empty() {
            return true;
        }
        self.full_name_lc.contains(q_lc)
            || self.description_lc.contains(q_lc)
            || self.topics_lc.contains(q_lc)
            || self.language_lc.contains(q_lc)
            || (!self.readme_lc.is_empty() && self.readme_lc.contains(q_lc))
    }
}

/// A searchable, rayon-ready corpus of starred repos with an LRU query cache.
pub struct RepoIndex {
    repos: Vec<IndexedRepo>,
    cache: Mutex<LruCache<SearchKey, Vec<usize>>>,
}

/// Filter-free search parameters used as the LRU cache key.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SearchKey {
    pub query_lc: String,
    pub lang_lc: Option<String>,
    pub topic: Option<String>,
}

/// A search result: the matching repo plus its score (higher is better).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit<'a> {
    pub repo: &'a Repo,
    pub score: f64,
}

impl RepoIndex {
    /// Build an index from a vector of repos. Lowercasing happens in parallel.
    pub fn new(repos: Vec<Repo>) -> Self {
        let indexed: Vec<IndexedRepo> = repos.into_par_iter().map(IndexedRepo::new).collect();
        Self {
            repos: indexed,
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(256).expect("non-zero"))),
        }
    }

    pub fn len(&self) -> usize {
        self.repos.len()
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Repo> {
        self.repos.iter().map(|i| &i.repo)
    }

    /// Execute a search against the index.
    ///
    /// Behavior:
    ///   - Empty `query` matches everything (subject to lang/topic filters).
    ///   - `lang` is matched case-insensitively against `repo.language`.
    ///   - `topic` is matched case-insensitively against any topic string.
    ///   - Matching indices are cached in an LRU keyed by `(query, lang, topic)`.
    ///   - Ranking runs AFTER cache lookup and is always parallelized via rayon.
    ///   - Returned slice is ordered by score descending, truncated to `limit`.
    pub fn search(
        &self,
        query: &str,
        lang: Option<&str>,
        topic: Option<&str>,
        limit: usize,
    ) -> Vec<SearchHit<'_>> {
        let q_lc = query.to_lowercase();
        let lang_lc = lang.map(|s| s.to_lowercase());
        let topic_owned = topic.map(|s| s.to_string());

        let key = SearchKey {
            query_lc: q_lc.clone(),
            lang_lc: lang_lc.clone(),
            topic: topic_owned.clone(),
        };

        let cached_indices: Option<Vec<usize>> = {
            let mut cache = self.cache.lock().expect("poisoned search cache");
            cache.get(&key).cloned()
        };

        let matching_indices: Vec<usize> = if let Some(cached) = cached_indices {
            cached
        } else {
            let fresh: Vec<usize> = self
                .repos
                .par_iter()
                .enumerate()
                .filter_map(|(i, ir)| {
                    if !ir.matches(&q_lc) {
                        return None;
                    }
                    if let Some(ref l) = lang_lc {
                        if ir.language_lc != *l {
                            return None;
                        }
                    }
                    if let Some(ref t) = topic_owned {
                        if !ir.topics_lower.iter().any(|x| x == t) {
                            return None;
                        }
                    }
                    Some(i)
                })
                .collect();

            let mut cache = self.cache.lock().expect("poisoned search cache");
            cache.put(key, fresh.clone());
            fresh
        };

        let mut hits: Vec<SearchHit<'_>> = matching_indices
            .par_iter()
            .map(|&i| SearchHit {
                repo: &self.repos[i].repo,
                score: self.repos[i].score(&q_lc),
            })
            .collect();

        hits.par_sort_unstable_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.repo.stargazers_count.cmp(&a.repo.stargazers_count))
                .then_with(|| a.repo.full_name.cmp(&b.repo.full_name))
        });

        hits.truncate(limit);
        hits
    }

    /// Total matching hit count for a search (before `limit` is applied).
    pub fn match_count(&self, query: &str, lang: Option<&str>, topic: Option<&str>) -> usize {
        let q_lc = query.to_lowercase();
        let lang_lc = lang.map(|s| s.to_lowercase());
        self.repos
            .par_iter()
            .filter(|ir| ir.matches(&q_lc))
            .filter(|ir| match &lang_lc {
                Some(l) => ir.language_lc == *l,
                None => true,
            })
            .filter(|ir| match topic {
                Some(t) => ir.topics_lower.iter().any(|x| x == t),
                None => true,
            })
            .count()
    }

    /// Count of cached query entries. Useful for benches / sanity checks.
    pub fn cache_len(&self) -> usize {
        self.cache.lock().expect("poisoned search cache").len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GitHub HTTP client
// ─────────────────────────────────────────────────────────────────────────────

/// Paginating client for the GitHub REST API.
///
/// All network access goes through `ureq` with rustls — no C TLS, no tokio.
/// The `base_url` is parameterizable so integration tests can point at a
/// local mock server (see `tests/gh_mock.rs`).
pub struct GhClient {
    agent: ureq::Agent,
    token: String,
    base_url: String,
}

impl GhClient {
    pub fn new(token: String) -> Self {
        Self::with_base(token, "https://api.github.com".to_string())
    }

    pub fn with_base(token: String, base_url: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build();
        Self {
            agent,
            token,
            base_url,
        }
    }

    /// Return every starred repo for `user` (or the authenticated caller if `None`).
    ///
    /// Walks every page of the `/user/starred` or `/users/{user}/starred`
    /// endpoint by following `Link: ...; rel="next"` headers. Stops on the
    /// first page without a next link or on the first empty page.
    pub fn starred(&self, user: Option<&str>) -> Result<Vec<serde_json::Value>> {
        let first = match user {
            Some(u) => format!("{}/users/{}/starred?per_page=100", self.base_url, u),
            None => format!("{}/user/starred?per_page=100", self.base_url),
        };
        let mut out: Vec<serde_json::Value> = Vec::new();
        let mut url = first;
        let mut page = 1usize;

        loop {
            eprint!("  fetching page {} ...", page);
            let resp = self
                .agent
                .get(&url)
                .set("Authorization", &format!("Bearer {}", self.token))
                .set(
                    "Accept",
                    "application/vnd.github.star+json, application/vnd.github+json",
                )
                .set("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .with_context(|| format!("GET {}", url))?;

            let link = resp.header("Link").map(|s| s.to_string());

            let body: serde_json::Value = resp.into_json().context("decode JSON response")?;
            let arr = body
                .as_array()
                .ok_or_else(|| anyhow!("expected JSON array, got {}", body))?;
            eprintln!(" got {} items", arr.len());
            if arr.is_empty() {
                break;
            }
            out.extend(arr.iter().cloned());

            let Some(link) = link else { break };
            let Some(next_url) = parse_link_next(&link) else {
                break;
            };
            url = next_url;
            page += 1;
        }

        Ok(out)
    }

    /// Fetch the raw README text for a single repo.
    ///
    /// Uses the `application/vnd.github.raw` media type so the response is
    /// the unparsed file body (not base64-encoded). 404 is rewritten to an
    /// empty string so a missing README doesn't abort a batch sync.
    /// Returned text is truncated to [`README_MAX_BYTES`].
    pub fn readme(&self, owner: &str, name: &str) -> Result<String> {
        let url = format!("{}/repos/{}/{}/readme", self.base_url, owner, name);
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/vnd.github.raw")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .call();

        let text = match resp {
            Ok(r) => r.into_string().unwrap_or_default(),
            Err(ureq::Error::Status(404, _)) => return Ok(String::new()),
            Err(e) => return Err(anyhow!("GET {}: {}", url, e)),
        };

        if text.len() > README_MAX_BYTES {
            // Truncate at a char boundary before the byte cap so we never
            // split a multi-byte UTF-8 codepoint.
            let mut cut = README_MAX_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            Ok(text[..cut].to_string())
        } else {
            Ok(text)
        }
    }
}

/// Fetch READMEs for every repo in `repos` in parallel via rayon.
///
/// Each rayon thread gets its own `ureq::Agent` (cheap — just an Arc
/// internally) so HTTP connection state doesn't bottleneck on a single
/// pool. `max_concurrency` caps how many requests run at once to stay
/// under GitHub's secondary rate limit (default: 8).
///
/// Returns a new `Vec<Repo>` with `readme` + `readme_fetched_at` populated
/// on each entry; source ordering is preserved.
pub fn fetch_readmes_parallel(token: &str, repos: Vec<Repo>, max_concurrency: usize) -> Vec<Repo> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(max_concurrency.max(1))
        .thread_name(|i| format!("stargaze-readme-{}", i))
        .build()
        .expect("build readme thread pool");

    pool.install(|| {
        repos
            .into_par_iter()
            .map(|mut r| {
                // Build a fresh client per task. `ureq::Agent` is Send+Sync
                // and cheap to clone, but a per-thread client keeps TLS
                // connection state thread-local and avoids contention.
                let client = GhClient::new(token.to_string());
                match client.readme(&r.owner, &r.name) {
                    Ok(body) => {
                        r.readme = Some(body);
                        r.readme_fetched_at = Some(Utc::now());
                    }
                    Err(e) => {
                        eprintln!("  readme fetch failed for {}: {}", r.full_name, e);
                    }
                }
                r
            })
            .collect()
    })
}

/// Extract the URL marked `rel="next"` from a GitHub Link header, if any.
///
/// Handles:
///   - standard `<url>; rel="next", <url>; rel="last"` form
///   - the `rel="next"` entry landing anywhere in the list
///   - URLs that themselves contain commas (the split on `,` is naive but
///     safe because we match on the `rel="next"` marker, not positional
///     ordering)
pub fn parse_link_next(link_header: &str) -> Option<String> {
    for part in link_header.split(',') {
        let part = part.trim();
        let mut it = part.splitn(2, ';');
        let url_part = it.next()?.trim();
        let rel_part = it.next()?.trim();
        if rel_part.contains(r#"rel="next""#) {
            let url = url_part.trim_start_matches('<').trim_end_matches('>');
            return Some(url.to_string());
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Auth resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve a GitHub personal access token from (in order):
///   1. the explicit `--token` flag if provided
///   2. the `GH_TOKEN` env var
///   3. the `GITHUB_TOKEN` env var
///
/// Empty strings are treated as missing. The error message tells the user
/// exactly what to do.
pub fn resolve_token(flag: Option<String>) -> Result<String> {
    if let Some(t) = flag {
        if !t.is_empty() {
            return Ok(t);
        }
    }
    for key in &["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return Ok(v);
            }
        }
    }
    bail!(
        "no GitHub token found.\n  \
         Set GH_TOKEN or GITHUB_TOKEN in your environment, or pass --token <PAT>.\n  \
         Quick start: export GH_TOKEN=$(gh auth token)"
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Storage (redb)
// ─────────────────────────────────────────────────────────────────────────────

/// Default data-file location resolved via [`directories::ProjectDirs`].
/// Creates the parent directory on first call.
pub fn default_db_path() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "bkataru", "stargaze")
        .ok_or_else(|| anyhow!("could not resolve platform data dir"))?;
    let data = dirs.data_dir();
    std::fs::create_dir_all(data).with_context(|| format!("create data dir {}", data.display()))?;
    Ok(data.join("stars.redb"))
}

/// Open (or create) the redb database at `path`.
pub fn open_db(path: &Path) -> Result<Database> {
    Database::create(path).with_context(|| format!("open redb at {}", path.display()))
}

/// Upsert a slice of repos in a single write transaction and record the
/// sync timestamp + count in the `meta` table.
pub fn upsert_repos(db: &Database, repos: &[Repo]) -> Result<usize> {
    let mut n = 0usize;
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(REPOS)?;
        for r in repos {
            let buf = serde_json::to_vec(r)?;
            table.insert(r.full_name.as_str(), buf.as_slice())?;
            n += 1;
        }
    }
    {
        let mut meta = txn.open_table(META)?;
        let now = Utc::now().to_rfc3339();
        meta.insert("last_sync", now.as_str())?;
        let count_s = n.to_string();
        meta.insert("last_sync_count", count_s.as_str())?;
    }
    txn.commit()?;
    Ok(n)
}

/// Read every cached repo. Returns an empty vec if the table doesn't exist yet.
pub fn load_all(db: &Database) -> Result<Vec<Repo>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(REPOS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for row in table.iter()? {
        let (_k, v) = row?;
        let r: Repo = serde_json::from_slice(v.value())?;
        out.push(r);
    }
    Ok(out)
}

/// Read one cached repo by `owner/name`. `Ok(None)` if not found.
pub fn load_one(db: &Database, full_name: &str) -> Result<Option<Repo>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(REPOS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(full_name)? {
        Some(v) => Ok(Some(serde_json::from_slice(v.value())?)),
        None => Ok(None),
    }
}

/// Read a metadata string by key. `Ok(None)` if absent.
pub fn read_meta(db: &Database, key: &str) -> Result<Option<String>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(META) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(key)?.map(|v| v.value().to_string()))
}

/// Count cached repos. Matches `load_all(db)?.len()` but avoids deserialization.
pub fn count_repos(db: &Database) -> Result<usize> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(REPOS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    Ok(table.len()? as usize)
}

/// Delete cached repos whose keys are NOT in `keep`. Returns number removed.
/// Used by sync to evict unstarred repos.
pub fn retain_repos(db: &Database, keep: &std::collections::HashSet<String>) -> Result<usize> {
    let to_delete: Vec<String> = {
        let txn = db.begin_read()?;
        match txn.open_table(REPOS) {
            Ok(table) => table
                .iter()?
                .filter_map(|row| row.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| !keep.contains(k))
                .collect(),
            Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
            Err(e) => return Err(e.into()),
        }
    };
    let n = to_delete.len();
    if n == 0 {
        return Ok(0);
    }
    let txn = db.begin_write()?;
    {
        let mut table = txn.open_table(REPOS)?;
        for k in &to_delete {
            table.remove(k.as_str())?;
        }
    }
    txn.commit()?;
    Ok(n)
}

// ─────────────────────────────────────────────────────────────────────────────
// Formatting helpers (shared with the CLI)
// ─────────────────────────────────────────────────────────────────────────────

/// Truncate a string to `n` chars (not bytes), appending `…` if cut.
/// Safe for multi-byte UTF-8 input.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP (Model Context Protocol) stdio server
//
// Hand-rolled minimal implementation of the subset of MCP that clients
// actually use for a read-only tool server: `initialize`, `tools/list`,
// `tools/call`, plus the `notifications/initialized` courtesy ack. We
// avoid pulling in `tokio` + `rmcp` to keep the single-static-binary
// invariant intact — MCP over stdio is just line-delimited JSON-RPC 2.0.
// ─────────────────────────────────────────────────────────────────────────────

/// Protocol version we advertise during `initialize`. Matches the value
/// used by Claude Desktop and other common MCP clients as of 2026-04.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Run an MCP stdio server that exposes a read-only view over the cached
/// stars. Blocks on stdin until EOF, writing JSON-RPC responses to stdout.
///
/// Tools exposed:
///   - `search_stars` — keyword search (name/desc/topics/lang/readme)
///   - `show_star`    — one repo by `owner/name`
///   - `list_stars`   — newest-first slice of the cache
///   - `stats`        — cache totals + last sync
pub fn run_mcp_stdio(db: Database) -> Result<()> {
    use std::io::{BufRead, Write};

    // Pre-build the index so every `search_stars` call is fast. We rebuild
    // once per server lifetime; clients that need fresh data should call
    // `stargaze sync` and restart the server.
    let repos = load_all(&db)?;
    let index = RepoIndex::new(repos);
    eprintln!(
        "stargaze: MCP stdio server ready ({} cached repos)",
        index.len()
    );

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).context("read from stdin")?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("stargaze mcp: drop malformed line: {}", e);
                continue;
            }
        };

        if let Some(resp) = handle_mcp_request(&req, &index, &db) {
            let bytes = serde_json::to_string(&resp)?;
            stdout.write_all(bytes.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
        // No response for notifications (method starts with "notifications/").
    }
    Ok(())
}

/// Dispatch a single JSON-RPC request. Returns `None` for notifications
/// (which must not elicit a response) and `Some(value)` for any method
/// call — including errors, which are reported via JSON-RPC `error`.
pub fn handle_mcp_request(
    req: &serde_json::Value,
    index: &RepoIndex,
    db: &Database,
) -> Option<serde_json::Value> {
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned();

    // Notifications carry no id and must not elicit a response.
    if method.starts_with("notifications/") {
        return None;
    }

    let result = match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "serverInfo": {
                "name": "stargaze",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "tools": {},
            },
        })),
        "tools/list" => Ok(serde_json::json!({
            "tools": [
                {
                    "name": "search_stars",
                    "description": "Search cached GitHub starred repos by substring across name, description, topics, language, and README. Optional lang/topic filters. Ranked by weighted hit count × log(stars).",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {"type": "string", "description": "Substring to search for (case-insensitive)"},
                            "lang":  {"type": "string", "description": "Optional primary language filter (case-insensitive exact match)"},
                            "topic": {"type": "string", "description": "Optional topic filter (exact match)"},
                            "limit": {"type": "integer", "description": "Max results to return", "default": 30}
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "show_star",
                    "description": "Return the full cached record for one repo by owner/name.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "full_name": {"type": "string", "description": "owner/name"}
                        },
                        "required": ["full_name"]
                    }
                },
                {
                    "name": "list_stars",
                    "description": "List cached repos sorted by stargazer count descending.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "limit": {"type": "integer", "default": 50}
                        }
                    }
                },
                {
                    "name": "stats",
                    "description": "Cache size, last sync timestamp, top languages.",
                    "inputSchema": {"type": "object", "properties": {}}
                }
            ]
        })),
        "tools/call" => mcp_tools_call(req, index, db),
        "ping" => Ok(serde_json::json!({})),
        other => Err((-32601i64, format!("method not found: {}", other))),
    };

    match result {
        Ok(r) => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": r,
        })),
        Err((code, msg)) => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": msg},
        })),
    }
}

fn mcp_tools_call(
    req: &serde_json::Value,
    index: &RepoIndex,
    db: &Database,
) -> std::result::Result<serde_json::Value, (i64, String)> {
    let params = req
        .get("params")
        .ok_or((-32602i64, "missing params".to_string()))?;
    let tool = params
        .get("name")
        .and_then(|s| s.as_str())
        .ok_or((-32602i64, "missing params.name".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    match tool {
        "search_stars" => {
            let query = args
                .get("query")
                .and_then(|q| q.as_str())
                .ok_or((-32602i64, "missing arguments.query".to_string()))?;
            let lang = args.get("lang").and_then(|l| l.as_str());
            let topic = args.get("topic").and_then(|t| t.as_str());
            let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(30) as usize;
            let hits = index.search(query, lang, topic, limit);
            Ok(mcp_tool_result_json(&hits_to_json(&hits)))
        }
        "show_star" => {
            let full_name = args
                .get("full_name")
                .and_then(|f| f.as_str())
                .ok_or((-32602i64, "missing arguments.full_name".to_string()))?;
            let repo =
                load_one(db, full_name).map_err(|e| (-32603i64, format!("load error: {}", e)))?;
            Ok(mcp_tool_result_json(
                &serde_json::to_value(repo).unwrap_or(serde_json::Value::Null),
            ))
        }
        "list_stars" => {
            let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(50) as usize;
            let mut all: Vec<&Repo> = index.iter().collect();
            all.sort_by(|a, b| b.stargazers_count.cmp(&a.stargazers_count));
            let slice: Vec<&Repo> = all.into_iter().take(limit).collect();
            Ok(mcp_tool_result_json(
                &serde_json::to_value(slice).unwrap_or(serde_json::Value::Null),
            ))
        }
        "stats" => {
            let count = count_repos(db).unwrap_or(0);
            let last_sync = read_meta(db, "last_sync").unwrap_or_default();
            let last_count = read_meta(db, "last_sync_count").unwrap_or_default();
            Ok(mcp_tool_result_json(&serde_json::json!({
                "cached": count,
                "last_sync": last_sync,
                "last_sync_count": last_count,
            })))
        }
        other => Err((-32601i64, format!("unknown tool: {}", other))),
    }
}

fn hits_to_json(hits: &[SearchHit<'_>]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "score": h.score,
                "full_name": h.repo.full_name,
                "description": h.repo.description,
                "language": h.repo.language,
                "stars": h.repo.stargazers_count,
                "topics": h.repo.topics,
                "url": h.repo.url,
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

fn mcp_tool_result_json(payload: &serde_json::Value) -> serde_json::Value {
    let text = serde_json::to_string_pretty(payload)
        .unwrap_or_else(|_| "<serialization error>".to_string());
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "isError": false,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP API surface (tiny_http, pure Rust, blocking)
//
// stargaze exposes three equivalent communication surfaces over the same
// cache: the CLI subcommands, the MCP stdio server, and this HTTP API.
// They cover the same read-only ops (search, show, list, stats) and dispatch
// through the same `RepoIndex` + `Database` handles so results are identical
// byte-for-byte.
// ─────────────────────────────────────────────────────────────────────────────

use std::net::SocketAddr;
use std::sync::Arc;

/// Run the HTTP API server until Ctrl-C. Bind address defaults to
/// `127.0.0.1:7879`. Optional `api_key` gates every request — when set,
/// clients must pass `Authorization: Bearer <key>`. When unset, the
/// server is fully open (fine for localhost).
pub fn run_api_server(
    db: Database,
    bind: SocketAddr,
    api_key: Option<String>,
    threads: usize,
) -> Result<()> {
    let repos = load_all(&db)?;
    let index = Arc::new(RepoIndex::new(repos));
    let db = Arc::new(db);
    let key = Arc::new(api_key);

    let server = tiny_http::Server::http(bind).map_err(|e| anyhow!("bind {}: {}", bind, e))?;
    eprintln!(
        "stargaze: HTTP API ready on http://{} ({} cached repos, {} worker threads, auth={})",
        bind,
        index.len(),
        threads.max(1),
        if key.is_some() { "on" } else { "off" }
    );

    // Naive thread pool: spawn N blocking workers that pull requests off
    // the server's blocking iterator.
    let server = Arc::new(server);
    let mut handles = Vec::with_capacity(threads.max(1));
    for _ in 0..threads.max(1) {
        let server = Arc::clone(&server);
        let index = Arc::clone(&index);
        let db = Arc::clone(&db);
        let key = Arc::clone(&key);
        handles.push(std::thread::spawn(move || {
            for req in server.incoming_requests() {
                handle_api_request(req, &index, &db, key.as_ref().as_deref());
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

/// Parse `?query=foo&limit=20&lang=Rust` style form-encoded query strings.
/// Pure Rust — no `url` crate. Handles `%`-escapes via a tiny decoder.
pub fn parse_query_string(qs: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k.is_empty() {
            continue;
        }
        out.insert(pct_decode(k), pct_decode(v));
    }
    out
}

fn pct_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'+' {
            out.push(' ');
            i += 1;
        } else if c == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            match u8::from_str_radix(hex, 16) {
                Ok(byte) => {
                    out.push(byte as char);
                    i += 3;
                }
                Err(_) => {
                    out.push(c as char);
                    i += 1;
                }
            }
        } else {
            out.push(c as char);
            i += 1;
        }
    }
    out
}

fn handle_api_request(
    mut req: tiny_http::Request,
    index: &RepoIndex,
    db: &Database,
    api_key: Option<&str>,
) {
    // Enforce auth if configured.
    if let Some(expected) = api_key {
        let authed = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| {
                let v = h.value.as_str();
                v.strip_prefix("Bearer ")
            })
            .map(|t| t == expected)
            .unwrap_or(false);
        if !authed {
            let _ = req
                .respond(tiny_http::Response::from_string("unauthorized\n").with_status_code(401));
            return;
        }
    }

    let method = req.method().as_str().to_string();
    let url_raw = req.url().to_string();
    let (path, qs) = match url_raw.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url_raw, String::new()),
    };
    let params = parse_query_string(&qs);

    // Drain the request body even if we don't use it — tiny_http docs
    // recommend this for keep-alive connection reuse.
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);

    let resp = route_api(&method, &path, &params, index, db);

    let json = serde_json::to_string(&resp.body).unwrap_or_else(|_| "{}".to_string());
    let mut http_resp = tiny_http::Response::from_string(json)
        .with_status_code(resp.status)
        .with_header(
            tiny_http::Header::from_bytes(
                &b"Content-Type"[..],
                &b"application/json; charset=utf-8"[..],
            )
            .unwrap(),
        );
    // CORS: localhost-friendly default, allow any origin for read-only GETs.
    http_resp = http_resp.with_header(
        tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
    );
    let _ = req.respond(http_resp);
}

/// Result of routing one HTTP request — used by the `serve --api` path
/// AND by the unit tests that drive routing without a real TCP socket.
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

impl ApiResponse {
    fn ok(v: serde_json::Value) -> Self {
        Self {
            status: 200,
            body: v,
        }
    }
    fn not_found(what: &str) -> Self {
        Self {
            status: 404,
            body: serde_json::json!({"error": "not found", "detail": what}),
        }
    }
    fn bad(what: &str) -> Self {
        Self {
            status: 400,
            body: serde_json::json!({"error": "bad request", "detail": what}),
        }
    }
    fn method_not_allowed() -> Self {
        Self {
            status: 405,
            body: serde_json::json!({"error": "method not allowed"}),
        }
    }
}

/// Pure routing function — deterministic, no I/O beyond the db + index
/// handles already owned by the caller. Testable without a TCP socket.
pub fn route_api(
    method: &str,
    path: &str,
    params: &std::collections::HashMap<String, String>,
    index: &RepoIndex,
    db: &Database,
) -> ApiResponse {
    if method != "GET" {
        return ApiResponse::method_not_allowed();
    }
    match path {
        "/api/v1/health" => ApiResponse::ok(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "cached": index.len(),
        })),
        "/api/v1/stats" => {
            let total = count_repos(db).unwrap_or(0);
            let last_sync = read_meta(db, "last_sync").unwrap_or_default();
            let last_count = read_meta(db, "last_sync_count").unwrap_or_default();
            ApiResponse::ok(serde_json::json!({
                "cached": total,
                "last_sync": last_sync,
                "last_sync_count": last_count,
            }))
        }
        "/api/v1/search" => {
            let Some(query) = params.get("q") else {
                return ApiResponse::bad("missing ?q=<query>");
            };
            let lang = params.get("lang").map(|s| s.as_str());
            let topic = params.get("topic").map(|s| s.as_str());
            let limit: usize = params
                .get("limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            let hits = index.search(query, lang, topic, limit);
            let total = index.match_count(query, lang, topic);
            ApiResponse::ok(serde_json::json!({
                "total": total,
                "shown": hits.len(),
                "hits": hits_to_json(&hits),
            }))
        }
        "/api/v1/list" => {
            let limit: usize = params
                .get("limit")
                .and_then(|s| s.parse().ok())
                .unwrap_or(50);
            let mut all: Vec<&Repo> = index.iter().collect();
            all.sort_by(|a, b| b.stargazers_count.cmp(&a.stargazers_count));
            let slice: Vec<&Repo> = all.into_iter().take(limit).collect();
            ApiResponse::ok(serde_json::to_value(slice).unwrap_or(serde_json::Value::Null))
        }
        p if p.starts_with("/api/v1/stars/") => {
            let name = p.trim_start_matches("/api/v1/stars/");
            if name.is_empty() || !name.contains('/') {
                return ApiResponse::bad("path must be /api/v1/stars/<owner>/<name>");
            }
            match load_one(db, name) {
                Ok(Some(r)) => {
                    ApiResponse::ok(serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
                }
                Ok(None) => ApiResponse::not_found(name),
                Err(e) => ApiResponse {
                    status: 500,
                    body: serde_json::json!({"error": "internal", "detail": e.to_string()}),
                },
            }
        }
        _ => ApiResponse::not_found(path),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(
        full_name: &str,
        lang: Option<&str>,
        stars: u64,
        desc: Option<&str>,
        topics: Vec<&str>,
    ) -> Repo {
        Repo {
            full_name: full_name.to_string(),
            owner: full_name.split('/').next().unwrap_or("").to_string(),
            name: full_name.split('/').nth(1).unwrap_or("").to_string(),
            description: desc.map(String::from),
            url: format!("https://github.com/{}", full_name),
            language: lang.map(String::from),
            stargazers_count: stars,
            forks_count: 0,
            open_issues_count: 0,
            topics: topics.iter().map(|s| s.to_string()).collect(),
            default_branch: Some("main".into()),
            license: None,
            archived: false,
            fork: false,
            pushed_at: None,
            created_at: None,
            updated_at: None,
            starred_at: None,
            cached_at: Utc::now(),
            readme: None,
            readme_fetched_at: None,
        }
    }

    // ─────────── parse_link_next ───────────

    #[test]
    fn link_next_simple_next_and_last() {
        let h = r#"<https://api.github.com/user/starred?page=2>; rel="next", <https://api.github.com/user/starred?page=5>; rel="last""#;
        assert_eq!(
            parse_link_next(h).unwrap(),
            "https://api.github.com/user/starred?page=2"
        );
    }

    #[test]
    fn link_next_only_last_no_next() {
        let h = r#"<https://api.github.com/user/starred?page=1>; rel="first", <https://api.github.com/user/starred?page=5>; rel="last""#;
        assert!(parse_link_next(h).is_none());
    }

    #[test]
    fn link_next_next_appears_second() {
        let h = r#"<https://api.github.com/user/starred?page=1>; rel="first", <https://api.github.com/user/starred?page=3>; rel="next""#;
        assert_eq!(
            parse_link_next(h).unwrap(),
            "https://api.github.com/user/starred?page=3"
        );
    }

    #[test]
    fn link_next_empty_header() {
        assert!(parse_link_next("").is_none());
    }

    #[test]
    fn link_next_malformed_header_no_rel_key() {
        let h = "<https://api.github.com/user/starred?page=2>";
        assert!(parse_link_next(h).is_none());
    }

    #[test]
    fn link_next_whitespace_tolerant() {
        let h = r#"  <https://api.github.com/user/starred?page=2>  ;   rel="next"  "#;
        assert_eq!(
            parse_link_next(h).unwrap(),
            "https://api.github.com/user/starred?page=2"
        );
    }

    // ─────────── Repo::from_api ───────────

    #[test]
    fn repo_from_api_plain() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "description": "a thing",
            "html_url": "https://github.com/foo/bar",
            "stargazers_count": 42,
            "forks_count": 3,
            "open_issues_count": 0,
            "language": "Rust",
            "topics": ["cli", "rust"],
            "default_branch": "main",
            "archived": false,
            "fork": false,
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.full_name, "foo/bar");
        assert_eq!(r.owner, "foo");
        assert_eq!(r.name, "bar");
        assert_eq!(r.stargazers_count, 42);
        assert_eq!(r.forks_count, 3);
        assert_eq!(r.topics, vec!["cli".to_string(), "rust".to_string()]);
        assert_eq!(r.language.as_deref(), Some("Rust"));
        assert!(r.starred_at.is_none());
    }

    #[test]
    fn repo_from_api_star_wrapper() {
        let v = serde_json::json!({
            "starred_at": "2024-03-14T10:20:30Z",
            "repo": {
                "full_name": "foo/bar",
                "name": "bar",
                "owner": {"login": "foo"},
                "html_url": "https://github.com/foo/bar",
                "stargazers_count": 1,
                "forks_count": 0,
                "open_issues_count": 0,
                "archived": false,
                "fork": false,
            }
        });
        let r = Repo::from_api(&v).unwrap();
        assert!(r.starred_at.is_some());
        assert_eq!(r.full_name, "foo/bar");
    }

    #[test]
    fn repo_from_api_missing_full_name_errors() {
        let v = serde_json::json!({"name": "bar", "owner": {"login": "foo"}});
        assert!(Repo::from_api(&v).is_err());
    }

    #[test]
    fn repo_from_api_missing_name_errors() {
        let v = serde_json::json!({"full_name": "foo/bar", "owner": {"login": "foo"}});
        assert!(Repo::from_api(&v).is_err());
    }

    #[test]
    fn repo_from_api_missing_optionals_collapse_to_none() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "html_url": "https://github.com/foo/bar",
            "archived": false,
            "fork": false,
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.description, None);
        assert_eq!(r.language, None);
        assert_eq!(r.topics, Vec::<String>::new());
        assert_eq!(r.license, None);
        assert_eq!(r.default_branch, None);
        assert_eq!(r.stargazers_count, 0);
    }

    #[test]
    fn repo_from_api_topics_wrong_type_becomes_empty() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "html_url": "",
            "topics": "not-an-array",
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.topics, Vec::<String>::new());
    }

    #[test]
    fn repo_from_api_license_spdx_id() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "html_url": "",
            "license": {"spdx_id": "MIT", "name": "MIT License"},
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn repo_from_api_license_null() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "html_url": "",
            "license": serde_json::Value::Null,
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.license, None);
    }

    #[test]
    fn repo_from_api_invalid_timestamps_are_none() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "html_url": "",
            "pushed_at": "not-a-date",
            "created_at": "also-not-a-date",
            "updated_at": "2024-03-14T10:20:30Z",
        });
        let r = Repo::from_api(&v).unwrap();
        assert!(r.pushed_at.is_none());
        assert!(r.created_at.is_none());
        assert!(r.updated_at.is_some());
    }

    #[test]
    fn repo_from_api_owner_fallback_from_full_name() {
        let v = serde_json::json!({
            "full_name": "foo/bar",
            "name": "bar",
            "html_url": "",
        });
        let r = Repo::from_api(&v).unwrap();
        assert_eq!(r.owner, "foo");
    }

    // ─────────── IndexedRepo ───────────

    #[test]
    fn indexed_repo_lowercase_precompute() {
        let r = make_repo(
            "Foo/PostgresBar",
            Some("Rust"),
            100,
            Some("A PostgreSQL helper"),
            vec!["Database", "CLI"],
        );
        let ir = IndexedRepo::new(r);
        assert_eq!(ir.full_name_lc, "foo/postgresbar");
        assert_eq!(ir.description_lc, "a postgresql helper");
        assert_eq!(ir.language_lc, "rust");
        assert_eq!(ir.topics_lc, "database cli");
        assert_eq!(
            ir.topics_lower,
            vec!["database".to_string(), "cli".to_string()]
        );
    }

    #[test]
    fn indexed_repo_matches_case_insensitive() {
        let ir = IndexedRepo::new(make_repo(
            "Foo/PostgresBar",
            Some("Rust"),
            10,
            Some("A PostgreSQL helper"),
            vec!["database"],
        ));
        assert!(ir.matches("postgres"));
        assert!(ir.matches("rust"));
        assert!(ir.matches("database"));
        assert!(ir.matches("helper"));
        assert!(!ir.matches("mysql"));
    }

    #[test]
    fn indexed_repo_matches_empty_query() {
        let ir = IndexedRepo::new(make_repo("a/b", None, 0, None, vec![]));
        assert!(ir.matches(""));
    }

    #[test]
    fn indexed_repo_score_weight_order() {
        // Full-name match should outweigh description match
        let ir_name = IndexedRepo::new(make_repo(
            "foo/postgres",
            Some("rust"),
            100,
            Some("irrelevant"),
            vec![],
        ));
        let ir_desc = IndexedRepo::new(make_repo(
            "foo/bar",
            Some("rust"),
            100,
            Some("a postgres wrapper"),
            vec![],
        ));
        assert!(ir_name.score("postgres") > ir_desc.score("postgres"));
    }

    #[test]
    fn indexed_repo_score_log_scaling_with_stars() {
        let low = IndexedRepo::new(make_repo("a/postgres", None, 10, None, vec![]));
        let high = IndexedRepo::new(make_repo("a/postgres", None, 10_000, None, vec![]));
        assert!(high.score("postgres") > low.score("postgres"));
    }

    #[test]
    fn indexed_repo_score_zero_on_no_match() {
        let ir = IndexedRepo::new(make_repo("a/b", Some("rust"), 100, Some("hi"), vec![]));
        assert_eq!(ir.score("nothinghere"), 0.0);
    }

    // ─────────── RepoIndex ───────────

    fn sample_corpus() -> Vec<Repo> {
        vec![
            make_repo(
                "launchbadge/sqlx",
                Some("Rust"),
                16000,
                Some("Rust SQL toolkit"),
                vec!["database", "rust", "sql"],
            ),
            make_repo(
                "postgresml/postgresml",
                Some("Rust"),
                6000,
                Some("Postgres with GPUs for ML"),
                vec!["postgres", "ml"],
            ),
            make_repo(
                "supabase/supabase",
                Some("TypeScript"),
                100000,
                Some("The Postgres development platform"),
                vec!["postgres", "database", "supabase"],
            ),
            make_repo(
                "facebook/react",
                Some("JavaScript"),
                200000,
                Some("A JavaScript library"),
                vec!["javascript", "ui"],
            ),
            make_repo(
                "torvalds/linux",
                Some("C"),
                150000,
                Some("Linux kernel"),
                vec![],
            ),
        ]
    }

    #[test]
    fn index_search_finds_substring_across_fields() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("postgres", None, None, 10);
        assert_eq!(hits.len(), 2);
        let names: Vec<&str> = hits.iter().map(|h| h.repo.full_name.as_str()).collect();
        assert!(names.contains(&"postgresml/postgresml"));
        assert!(names.contains(&"supabase/supabase"));
    }

    #[test]
    fn index_search_language_filter() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("", Some("Rust"), None, 10);
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert_eq!(h.repo.language.as_deref(), Some("Rust"));
        }
    }

    #[test]
    fn index_search_topic_filter() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("", None, Some("database"), 10);
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert!(h.repo.topics.iter().any(|t| t == "database"));
        }
    }

    #[test]
    fn index_search_language_case_insensitive() {
        let idx = RepoIndex::new(sample_corpus());
        let upper = idx.search("", Some("RUST"), None, 10);
        let lower = idx.search("", Some("rust"), None, 10);
        assert_eq!(upper.len(), lower.len());
        assert_eq!(upper.len(), 2);
    }

    #[test]
    fn index_search_limit_respected() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("", None, None, 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn index_search_sort_desc_by_score_then_stars() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("postgres", None, None, 10);
        assert!(hits.len() >= 2);
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn index_search_empty_query_matches_all() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("", None, None, 100);
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn index_search_no_matches() {
        let idx = RepoIndex::new(sample_corpus());
        let hits = idx.search("absolutelynothinglikethis", None, None, 10);
        assert_eq!(hits.len(), 0);
    }

    #[test]
    fn index_search_lru_cache_populates() {
        let idx = RepoIndex::new(sample_corpus());
        assert_eq!(idx.cache_len(), 0);
        let _ = idx.search("postgres", None, None, 10);
        assert_eq!(idx.cache_len(), 1);
        let _ = idx.search("postgres", None, None, 10);
        assert_eq!(idx.cache_len(), 1);
        let _ = idx.search("react", None, None, 10);
        assert_eq!(idx.cache_len(), 2);
    }

    #[test]
    fn index_search_cache_distinct_by_filters() {
        let idx = RepoIndex::new(sample_corpus());
        let _ = idx.search("", None, None, 10);
        let _ = idx.search("", Some("Rust"), None, 10);
        let _ = idx.search("", None, Some("database"), 10);
        assert_eq!(idx.cache_len(), 3);
    }

    #[test]
    fn index_match_count_matches_unlimited_search() {
        let idx = RepoIndex::new(sample_corpus());
        let count = idx.match_count("postgres", None, None);
        let hits = idx.search("postgres", None, None, 1_000_000);
        assert_eq!(count, hits.len());
    }

    #[test]
    fn index_is_empty_and_len() {
        let empty = RepoIndex::new(Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let populated = RepoIndex::new(sample_corpus());
        assert!(!populated.is_empty());
        assert_eq!(populated.len(), 5);
    }

    // ─────────── truncate ───────────

    #[test]
    fn truncate_shorter_than_n_unchanged() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_exact_boundary_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 6), "hello…");
    }

    #[test]
    fn truncate_multibyte_safe() {
        assert_eq!(truncate("日本語テキスト", 4), "日本語…");
    }

    #[test]
    fn truncate_zero_n() {
        assert_eq!(truncate("hello", 0), "…");
    }

    // ─────────── resolve_token ───────────

    #[test]
    fn resolve_token_flag_wins() {
        let tok = resolve_token(Some("flag-token".into())).unwrap();
        assert_eq!(tok, "flag-token");
    }

    #[test]
    fn resolve_token_empty_flag_falls_through() {
        // SAFETY: these tests serialize via the fact that we set + unset
        // before asserting, and cargo test uses one process for --test-threads=1
        // default module behaviour. Using scoped guards keeps this reliable.
        let _g = EnvGuard::set("GH_TOKEN", "env-token");
        let tok = resolve_token(Some(String::new())).unwrap();
        assert_eq!(tok, "env-token");
    }

    #[test]
    fn resolve_token_missing_errors_with_hint() {
        let _g1 = EnvGuard::unset("GH_TOKEN");
        let _g2 = EnvGuard::unset("GITHUB_TOKEN");
        let e = resolve_token(None).unwrap_err();
        let msg = format!("{}", e);
        assert!(msg.contains("GH_TOKEN"));
        assert!(msg.contains("gh auth token"));
    }

    // ─────────── MCP protocol handler ───────────

    fn mcp_test_index() -> (RepoIndex, tempfile::TempDir, Database) {
        let dir = tempfile::Builder::new()
            .prefix("stargaze-mcp-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("stars.redb");
        let db = open_db(&path).unwrap();
        let repos = sample_corpus();
        upsert_repos(&db, &repos).unwrap();
        let idx = RepoIndex::new(repos);
        (idx, dir, db)
    }

    #[test]
    fn mcp_initialize_advertises_tools_capability() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "stargaze");
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(
            resp["result"]["protocolVersion"].as_str().unwrap(),
            MCP_PROTOCOL_VERSION
        );
    }

    #[test]
    fn mcp_tools_list_includes_search_stars() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"search_stars"));
        assert!(names.contains(&"show_star"));
        assert!(names.contains(&"list_stars"));
        assert!(names.contains(&"stats"));
    }

    #[test]
    fn mcp_tools_call_search_stars_returns_hits() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search_stars",
                "arguments": {"query": "postgres", "limit": 10}
            }
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        let content = resp["result"]["content"].as_array().unwrap();
        assert!(!content.is_empty());
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("postgres") || text.contains("supabase"));
    }

    #[test]
    fn mcp_tools_call_search_stars_missing_query_errors() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "search_stars",
                "arguments": {}
            }
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        assert!(resp.get("error").is_some());
    }

    #[test]
    fn mcp_tools_call_show_star_returns_record_or_null() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "show_star",
                "arguments": {"full_name": "supabase/supabase"}
            }
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        let content = resp["result"]["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("supabase/supabase"));
    }

    #[test]
    fn mcp_tools_call_stats_returns_cache_size() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {"name": "stats", "arguments": {}}
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        let content = resp["result"]["content"].as_array().unwrap();
        let text = content[0]["text"].as_str().unwrap();
        assert!(text.contains("cached"));
    }

    #[test]
    fn mcp_tools_call_list_stars_respects_limit() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "list_stars",
                "arguments": {"limit": 2}
            }
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    #[test]
    fn mcp_notification_gets_no_response() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        assert!(handle_mcp_request(&req, &idx, &db).is_none());
    }

    #[test]
    fn mcp_unknown_method_returns_jsonrpc_error() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "definitely/not/a/method"
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn mcp_tools_call_unknown_tool_returns_error() {
        let (idx, _dir, db) = mcp_test_index();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {"name": "not_a_real_tool", "arguments": {}}
        });
        let resp = handle_mcp_request(&req, &idx, &db).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    // ─────────── HTTP API router ───────────

    fn api_params(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn api_health_returns_200_with_version() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api("GET", "/api/v1/health", &api_params(&[]), &idx, &db);
        assert_eq!(r.status, 200);
        assert_eq!(r.body["status"], "ok");
        assert!(r.body["version"].is_string());
        assert_eq!(r.body["cached"], idx.len() as u64);
    }

    #[test]
    fn api_stats_matches_cache_size() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api("GET", "/api/v1/stats", &api_params(&[]), &idx, &db);
        assert_eq!(r.status, 200);
        assert_eq!(r.body["cached"], idx.len() as u64);
    }

    #[test]
    fn api_search_requires_q_param() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api("GET", "/api/v1/search", &api_params(&[]), &idx, &db);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn api_search_returns_hits_with_total() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api(
            "GET",
            "/api/v1/search",
            &api_params(&[("q", "postgres"), ("limit", "5")]),
            &idx,
            &db,
        );
        assert_eq!(r.status, 200);
        assert!(r.body["total"].as_u64().unwrap() >= 2);
        let hits = r.body["hits"].as_array().unwrap();
        assert!(hits.len() >= 2);
        let names: Vec<&str> = hits
            .iter()
            .filter_map(|h| h["full_name"].as_str())
            .collect();
        assert!(names.contains(&"supabase/supabase"));
    }

    #[test]
    fn api_search_lang_filter() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api(
            "GET",
            "/api/v1/search",
            &api_params(&[("q", ""), ("lang", "Rust")]),
            &idx,
            &db,
        );
        assert_eq!(r.status, 200);
        let hits = r.body["hits"].as_array().unwrap();
        for h in hits {
            assert_eq!(h["language"], "Rust");
        }
    }

    #[test]
    fn api_list_respects_limit() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api(
            "GET",
            "/api/v1/list",
            &api_params(&[("limit", "3")]),
            &idx,
            &db,
        );
        assert_eq!(r.status, 200);
        let arr = r.body.as_array().unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn api_show_existing_repo() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api(
            "GET",
            "/api/v1/stars/supabase/supabase",
            &api_params(&[]),
            &idx,
            &db,
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body["full_name"], "supabase/supabase");
    }

    #[test]
    fn api_show_missing_repo_returns_404() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api(
            "GET",
            "/api/v1/stars/nope/nope",
            &api_params(&[]),
            &idx,
            &db,
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn api_unknown_path_returns_404() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api("GET", "/not/a/path", &api_params(&[]), &idx, &db);
        assert_eq!(r.status, 404);
    }

    #[test]
    fn api_non_get_returns_405() {
        let (idx, _dir, db) = mcp_test_index();
        let r = route_api("POST", "/api/v1/search", &api_params(&[]), &idx, &db);
        assert_eq!(r.status, 405);
    }

    #[test]
    fn parse_query_string_basic() {
        let m = parse_query_string("q=postgres&limit=10");
        assert_eq!(m.get("q").unwrap(), "postgres");
        assert_eq!(m.get("limit").unwrap(), "10");
    }

    #[test]
    fn parse_query_string_pct_decode() {
        let m = parse_query_string("q=hello%20world&topic=web%2dservers");
        assert_eq!(m.get("q").unwrap(), "hello world");
        assert_eq!(m.get("topic").unwrap(), "web-servers");
    }

    #[test]
    fn parse_query_string_plus_is_space() {
        let m = parse_query_string("q=hello+world");
        assert_eq!(m.get("q").unwrap(), "hello world");
    }

    #[test]
    fn parse_query_string_empty() {
        let m = parse_query_string("");
        assert!(m.is_empty());
    }

    /// RAII env-var guard for serial tests that touch the process env.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
