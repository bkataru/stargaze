//! stargaze — CLI entry point.
//!
//! Thin shell over [`stargaze`] (the library). All logic lives in `lib.rs`
//! so it's reachable from `tests/` integration tests and `benches/`
//! criterion benches.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use stargaze::{
    count_repos, default_db_path, fetch_readmes_parallel, load_all, load_one, open_db, read_meta,
    resolve_token, retain_repos, run_api_server, run_mcp_stdio, truncate, upsert_repos, GhClient,
    Repo, RepoIndex, SearchHit,
};

#[derive(Parser)]
#[command(
    name = "stargaze",
    version,
    about = "Cache and search your GitHub stars"
)]
struct Cli {
    /// Override the database path (default: $XDG_DATA_HOME/stargaze/stars.redb)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// GitHub personal access token (falls back to GH_TOKEN / GITHUB_TOKEN env var)
    #[arg(long, global = true)]
    token: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fetch all starred repos from GitHub and upsert into the local cache.
    Sync {
        /// GitHub username whose stars to sync (default: authenticated user)
        #[arg(short, long)]
        user: Option<String>,
        /// Also delete locally-cached repos that are no longer starred.
        #[arg(long, default_value_t = false)]
        prune: bool,
        /// Also fetch README text for every repo (costs ~1 request per star).
        #[arg(long, default_value_t = false)]
        with_readmes: bool,
        /// Max concurrent README fetches when `--with-readmes` is set.
        #[arg(long, default_value_t = 8)]
        concurrency: usize,
    },
    /// Fetch READMEs for already-cached repos (skips those already fetched).
    Readmes {
        /// Max concurrent fetches.
        #[arg(short, long, default_value_t = 8)]
        concurrency: usize,
        /// Refetch READMEs even if already cached.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Keyword search across cached stars (includes README text if fetched).
    Search {
        /// Query string (matched case-insensitively)
        query: String,
        /// Max results to show
        #[arg(short, long, default_value_t = 30)]
        limit: usize,
        /// Restrict by primary language (case-insensitive)
        #[arg(long)]
        lang: Option<String>,
        /// Restrict by topic (exact match, case-insensitive)
        #[arg(long)]
        topic: Option<String>,
    },
    /// Show the full cached record for a specific repo.
    Show {
        /// `owner/name`
        full_name: String,
    },
    /// Print cache stats (total repos, last-sync time, top languages).
    Stats,
    /// List all cached repos sorted by stargazer count.
    List {
        #[arg(short, long, default_value_t = 50)]
        limit: usize,
    },
    /// Run a Model Context Protocol (MCP) stdio server over the cache.
    ///
    /// Exposes `search_stars`, `show_star`, `list_stars`, and `stats` as
    /// MCP tools, so any MCP-compatible client (Claude Desktop, Cursor,
    /// custom agents) can query your starred repos as structured data.
    Serve,
    /// Run an HTTP JSON API server over the cache.
    ///
    /// Same read-only surface as the MCP server: search, show, list,
    /// stats, health. Binds to 127.0.0.1 by default. Pass `--api-key` to
    /// require `Authorization: Bearer <key>` on every request.
    Api {
        /// Address to bind (default 127.0.0.1:7879).
        #[arg(long, default_value = "127.0.0.1:7879")]
        bind: String,
        /// Optional bearer-token API key. When set, every request must
        /// include `Authorization: Bearer <key>`.
        #[arg(long)]
        api_key: Option<String>,
        /// Number of blocking worker threads.
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };
    let db = open_db(&db_path)?;

    match cli.cmd {
        Cmd::Sync {
            user,
            prune,
            with_readmes,
            concurrency,
        } => {
            let token = resolve_token(cli.token)?;
            cmd_sync(&db, token, user, prune, with_readmes, concurrency)
        }
        Cmd::Readmes { concurrency, force } => {
            let token = resolve_token(cli.token)?;
            cmd_readmes(&db, token, concurrency, force)
        }
        Cmd::Search {
            query,
            limit,
            lang,
            topic,
        } => cmd_search(&db, &query, limit, lang, topic),
        Cmd::Show { full_name } => cmd_show(&db, &full_name),
        Cmd::Stats => cmd_stats(&db),
        Cmd::List { limit } => cmd_list(&db, limit),
        Cmd::Serve => run_mcp_stdio(db),
        Cmd::Api {
            bind,
            api_key,
            threads,
        } => {
            let addr: std::net::SocketAddr = bind
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --bind {}: {}", bind, e))?;
            run_api_server(db, addr, api_key, threads)
        }
    }
}

fn cmd_sync(
    db: &redb::Database,
    token: String,
    user: Option<String>,
    prune: bool,
    with_readmes: bool,
    concurrency: usize,
) -> Result<()> {
    eprintln!("stargaze: syncing stars from github.com ...");
    let client = GhClient::new(token.clone());
    let items = client.starred(user.as_deref())?;
    eprintln!("stargaze: fetched {} raw items", items.len());

    let mut repos = Vec::with_capacity(items.len());
    for item in &items {
        match Repo::from_api(item) {
            Ok(r) => repos.push(r),
            Err(e) => eprintln!("  skip: {}", e),
        }
    }

    if with_readmes {
        eprintln!(
            "stargaze: fetching {} READMEs in parallel (concurrency={}) ...",
            repos.len(),
            concurrency
        );
        repos = fetch_readmes_parallel(&token, repos, concurrency);
        let fetched = repos.iter().filter(|r| r.readme.is_some()).count();
        eprintln!("stargaze: fetched {} READMEs", fetched);
    }

    let n = upsert_repos(db, &repos)?;
    eprintln!("stargaze: upserted {} repos", n);

    if prune {
        let keep: std::collections::HashSet<String> =
            repos.iter().map(|r| r.full_name.clone()).collect();
        let removed = retain_repos(db, &keep)?;
        if removed > 0 {
            eprintln!("stargaze: pruned {} unstarred repos", removed);
        }
    }
    Ok(())
}

fn cmd_readmes(db: &redb::Database, token: String, concurrency: usize, force: bool) -> Result<()> {
    let all = load_all(db)?;
    let targets: Vec<Repo> = if force {
        all
    } else {
        all.into_iter().filter(|r| r.readme.is_none()).collect()
    };
    if targets.is_empty() {
        eprintln!("stargaze: nothing to fetch (all READMEs cached)");
        return Ok(());
    }
    eprintln!(
        "stargaze: fetching {} READMEs in parallel (concurrency={}) ...",
        targets.len(),
        concurrency
    );
    let fetched = fetch_readmes_parallel(&token, targets, concurrency);
    let upserted = upsert_repos(db, &fetched)?;
    let hit = fetched.iter().filter(|r| r.readme.is_some()).count();
    eprintln!(
        "stargaze: upserted {} repos ({} with fresh README)",
        upserted, hit
    );
    Ok(())
}

fn cmd_search(
    db: &redb::Database,
    query: &str,
    limit: usize,
    lang: Option<String>,
    topic: Option<String>,
) -> Result<()> {
    let repos = load_all(db)?;
    if repos.is_empty() {
        eprintln!("(cache is empty — run `stargaze sync` first)");
        return Ok(());
    }
    let idx = RepoIndex::new(repos);
    let hits = idx.search(query, lang.as_deref(), topic.as_deref(), limit);

    if hits.is_empty() {
        println!("(no matches for {:?})", query);
        return Ok(());
    }
    for h in &hits {
        print_hit(h);
    }
    let total = idx.match_count(query, lang.as_deref(), topic.as_deref());
    println!();
    println!("{} match(es), showing {}", total, hits.len());
    Ok(())
}

fn print_hit(h: &SearchHit<'_>) {
    let r = h.repo;
    let lang = r.language.as_deref().unwrap_or("-");
    let desc = r.description.as_deref().unwrap_or("");
    let desc_trunc: String = desc.chars().take(100).collect();
    println!(
        "  {:<50} {:<12} ★{:<7} {}",
        truncate(&r.full_name, 50),
        truncate(lang, 12),
        r.stargazers_count,
        desc_trunc
    );
}

fn cmd_show(db: &redb::Database, full_name: &str) -> Result<()> {
    match load_one(db, full_name)? {
        Some(r) => {
            println!("{}", serde_json::to_string_pretty(&r)?);
            Ok(())
        }
        None => {
            eprintln!("(not in cache — run `stargaze sync` first)");
            std::process::exit(1);
        }
    }
}

fn cmd_stats(db: &redb::Database) -> Result<()> {
    let total = count_repos(db)?;
    let last_sync = read_meta(db, "last_sync")?.unwrap_or_else(|| "(never)".into());
    let last_count = read_meta(db, "last_sync_count")?.unwrap_or_else(|| "0".into());
    println!("stargaze stats");
    println!("  cached repos : {}", total);
    println!("  last sync    : {}", last_sync);
    println!("  last sync n  : {}", last_count);

    let repos = load_all(db)?;
    let mut by_lang: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for r in &repos {
        let l = r.language.as_deref().unwrap_or("-");
        *by_lang.entry(l).or_insert(0) += 1;
    }
    let mut langs: Vec<_> = by_lang.into_iter().collect();
    langs.sort_by(|a, b| b.1.cmp(&a.1));
    println!("  top languages:");
    for (l, c) in langs.iter().take(10) {
        println!("    {:<15} {}", l, c);
    }
    Ok(())
}

fn cmd_list(db: &redb::Database, limit: usize) -> Result<()> {
    let mut all = load_all(db)?;
    all.sort_by(|a, b| b.stargazers_count.cmp(&a.stargazers_count));
    for r in all.iter().take(limit) {
        let lang = r.language.as_deref().unwrap_or("-");
        let desc = r.description.as_deref().unwrap_or("");
        let desc_trunc: String = desc.chars().take(100).collect();
        println!(
            "  {:<50} {:<12} ★{:<7} {}",
            truncate(&r.full_name, 50),
            truncate(lang, 12),
            r.stargazers_count,
            desc_trunc
        );
    }
    Ok(())
}
