//! stargaze — CLI entry point.
//!
//! Thin shell over [`stargaze`] (the library). All logic lives in `lib.rs`
//! so it's reachable from `tests/` integration tests and `benches/`
//! criterion benches.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use redb::Database;
use std::path::PathBuf;

use stargaze::{
    cmd_list, cmd_readmes, cmd_search, cmd_show, cmd_stats, cmd_sync, count_repos, default_db_path,
    fetch_readmes_parallel, load_all, load_one, open_db, read_meta,
    resolve_token, retain_repos, run_api_server, run_mcp_stdio, truncate, upsert_repos,
    regenerate_embeddings, GhClient, Repo, RepoIndex, SearchHit,
};
use async_std::task::spawn_blocking;

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
        /// Use fuzzy matching (instead of substring)
        #[arg(long, default_value_t = false)]
        fuzzy: bool,
        /// Match any term (OR) instead of all terms (AND)
        #[arg(long, default_value_t = false)]
        or_mode: bool,
        /// Boost score when topic matches exactly
        #[arg(long, default_value_t = false)]
        topic_boost: bool,
        /// Use semantic search (keyword-based boost)
        #[arg(long, default_value_t = false)]
        semantic: bool,
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
    /// Regenerate embeddings for repos that don't have one.
    Embed {
        /// Max concurrent embedding generations.
        #[arg(short, long, default_value_t = 4)]
        concurrency: usize,
    },
}

#[async_std::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = match cli.db {
        Some(p) => p,
        None => default_db_path()?,
    };
    let db = Arc::new(open_db(&db_path).expect("failed to open db"));

    match cli.cmd {
        Cmd::Sync {
            user,
            prune,
            with_readmes,
            concurrency,
        } => {
            let token = resolve_token(cli.token)?;
            stargaze::cmd_sync(Arc::clone(&db), token, user, prune, with_readmes, concurrency).await
        }
        Cmd::Readmes { concurrency, force } => {
            let token = resolve_token(cli.token)?;
            stargaze::cmd_readmes(Arc::clone(&db), token, concurrency, force).await
        }
        Cmd::Search {
            query,
            limit,
            lang,
            topic,
            fuzzy,
            or_mode,
            topic_boost,
            semantic,
        } => stargaze::cmd_search(Arc::clone(&db), &query, limit, lang, topic, fuzzy, or_mode, topic_boost, semantic).await,
        Cmd::Show { full_name } => stargaze::cmd_show(Arc::clone(&db), &full_name).await,
        Cmd::Stats => stargaze::cmd_stats(Arc::clone(&db)).await,
        Cmd::List { limit } => stargaze::cmd_list(Arc::clone(&db), limit).await,
Cmd::Serve => { spawn_blocking(move || run_mcp_stdio(Arc::try_unwrap(db).unwrap_or_else(|_| panic!("Failed to unwrap Arc")))).await?; Ok(()) },
        Cmd::Api {
            bind,
            api_key,
            threads,
        } => {
            let addr: std::net::SocketAddr = bind
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid --bind {}: {}", bind, e))?;
            spawn_blocking(move || run_api_server(Arc::try_unwrap(db).unwrap_or_else(|_| panic!("Failed to unwrap Arc")), addr, api_key, threads)).await?; Ok(())
        }
        Cmd::Embed { concurrency: _ } => {
            eprintln!("Regenerating embeddings for repos missing them...");
            let (updated, skipped, errors) = regenerate_embeddings(&db)
                .map_err(|e| anyhow::anyhow!("Failed to regenerate embeddings: {}", e))?;
            eprintln!("Done: {} updated, {} skipped (already had embeddings), {} errors", updated, skipped, errors);
            Ok(())
        }
    }
}
