//! Live tests that hit the real GitHub API.
//!
//! These are `#[ignore]` by default so `cargo test` stays hermetic. To run
//! them:
//!
//! ```bash
//! export GH_TOKEN=$(gh auth token)
//! cargo test -- --ignored
//! # or a single test:
//! cargo test live_starred_first_page -- --ignored
//! ```
//!
//! Any test that finds `GH_TOKEN` empty short-circuits with a skip
//! message so an opted-in run on a stale environment doesn't hard-fail.

use stargaze::{
    count_repos, fetch_readmes_parallel, load_all, open_db, upsert_repos, GhClient, Repo, RepoIndex,
};

fn token() -> Option<String> {
    let t = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .ok()?;
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn skip_if_no_token(test_name: &str) -> Option<String> {
    match token() {
        Some(t) => Some(t),
        None => {
            eprintln!(
                "[live::{}] SKIPPED — GH_TOKEN not set. \
                 Run with `export GH_TOKEN=$(gh auth token)` then \
                 `cargo test -- --ignored`.",
                test_name
            );
            None
        }
    }
}

#[async_std::test]
#[ignore]
async fn live_starred_first_page() {
    let Some(t) = skip_if_no_token("live_starred_first_page") else {
        return;
    };
    let client = GhClient::new(t);
    // Hitting `/user/starred` requires a token that owns stars; that's our
    // `bkataru` setup. Just make sure at least one page comes back.
    let items = client.starred(None).await.expect("live starred call");
    assert!(
        !items.is_empty(),
        "expected at least one starred repo, got zero"
    );
    let first = &items[0];
    assert!(
        first.get("full_name").is_some() || first.get("repo").is_some(),
        "first item shape unexpected: {}",
        first
    );
}

#[async_std::test]
#[ignore]
async fn live_starred_public_user() {
    let Some(t) = skip_if_no_token("live_starred_public_user") else {
        return;
    };
    let client = GhClient::new(t);
    // Public starred list for a known account — doesn't depend on the
    // authenticated user's actual stars, just validates pagination.
    let items = client
        .starred(Some("rust-lang-nursery"))
        .await
        .expect("live public starred");
    // `rust-lang-nursery` is an org without stars, so zero results is
    // acceptable; we're just asserting the call path works end-to-end.
    assert!(
        items.len() < 10_000,
        "unexpected payload size from public-user stars: {}",
        items.len()
    );
}

#[async_std::test]
#[ignore]
async fn live_readme_fetch_rust_lang_rust() {
    let Some(t) = skip_if_no_token("live_readme_fetch_rust_lang_rust") else {
        return;
    };
    let client = GhClient::new(t);
    let body = client.readme("rust-lang", "rust").await.expect("fetch README");
    assert!(!body.is_empty(), "expected non-empty README");
    assert!(
        body.to_lowercase().contains("rust"),
        "rust-lang/rust README should contain the word 'rust'"
    );
}

#[async_std::test]
#[ignore]
async fn live_readme_fetch_missing_repo_returns_empty() {
    let Some(t) = skip_if_no_token("live_readme_fetch_missing_repo_returns_empty") else {
        return;
    };
    let client = GhClient::new(t);
    // This name should not exist. If GitHub starts parking it we'll need
    // to pick a fresher sentinel.
    let body = client
        .readme("bkataru", "stargaze-definitely-nonexistent-sentinel-9d8f7a")
        .await
        .expect("404 should not error");
    assert_eq!(body, "", "missing repos should return empty, not fail");
}

#[async_std::test]
#[ignore]
async fn live_parallel_readme_batch() {
    let Some(t) = skip_if_no_token("live_parallel_readme_batch") else {
        return;
    };
    let targets = vec![
        ("rust-lang", "rust"),
        ("tokio-rs", "tokio"),
        ("launchbadge", "sqlx"),
    ];
    let repos: Vec<Repo> = targets
        .into_iter()
        .map(|(owner, name)| Repo {
            full_name: format!("{}/{}", owner, name),
            owner: owner.to_string(),
            name: name.to_string(),
            description: None,
            url: format!("https://github.com/{}/{}", owner, name),
            language: None,
            stargazers_count: 0,
            forks_count: 0,
            open_issues_count: 0,
            topics: vec![],
            default_branch: None,
            license: None,
            archived: false,
            fork: false,
            pushed_at: None,
            created_at: None,
            updated_at: None,
            starred_at: None,
            cached_at: chrono::Utc::now(),
            readme: None,
            readme_fetched_at: None,
            embedding: None,
        })
        .collect();
    let fetched = fetch_readmes_parallel(&t, repos, 3).await;
    assert_eq!(fetched.len(), 3);
    let hit = fetched.iter().filter(|r| r.readme.is_some()).count();
    assert_eq!(hit, 3, "all 3 well-known repos should return a README");
    for r in &fetched {
        let body = r.readme.as_deref().unwrap_or("");
        assert!(!body.is_empty(), "{}: empty README", r.full_name);
    }
}

#[async_std::test]
#[ignore]
async fn live_mini_sync_roundtrip() {
    let Some(t) = skip_if_no_token("live_mini_sync_roundtrip") else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("stargaze-live-")
        .tempdir()
        .expect("tempdir");
    let db_path = dir.path().join("stars.redb");
    let db = open_db(&db_path).expect("open_db");

    let client = GhClient::new(t);
    // For speed, only sync via user's own stars — pagination covered in
    // live_starred_first_page. Here we test the *sync pipeline* end-to-end
    // against a real DB.
    let items = client.starred(None).await.expect("live starred");
    let mut repos = Vec::new();
    for item in items.iter().take(50) {
        if let Ok(r) = Repo::from_api(item) {
            repos.push(r);
        }
    }
    assert!(!repos.is_empty(), "should have parsed at least one repo");
    upsert_repos(&db, &repos).expect("upsert");
    assert_eq!(count_repos(&db).unwrap(), repos.len());

    let loaded = load_all(&db).expect("load_all");
    let idx = RepoIndex::new(loaded);
    assert_eq!(idx.len(), repos.len());
    let hits = idx.search("", None, None, 1_000_000, false, false, false, false);
    assert_eq!(hits.len(), repos.len());
}
