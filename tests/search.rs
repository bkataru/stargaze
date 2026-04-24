//! Integration tests for the search + ranking pipeline.

use chrono::Utc;
use stargaze::{Repo, RepoIndex};

fn r(
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
        default_branch: None,
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
        embedding: None,
    }
}

fn corpus() -> Vec<Repo> {
    vec![
        r(
            "launchbadge/sqlx",
            Some("Rust"),
            16_000,
            Some("Rust SQL toolkit"),
            vec!["database", "rust", "sql"],
        ),
        r(
            "postgresml/postgresml",
            Some("Rust"),
            6_000,
            Some("Postgres with GPUs for ML"),
            vec!["postgres", "ml"],
        ),
        r(
            "supabase/supabase",
            Some("TypeScript"),
            100_000,
            Some("The Postgres development platform"),
            vec!["postgres", "database", "supabase"],
        ),
        r(
            "grafana/grafana",
            Some("TypeScript"),
            70_000,
            Some("Observability and data visualization platform"),
            vec!["dashboard", "monitoring"],
        ),
        r(
            "prisma/prisma",
            Some("TypeScript"),
            45_000,
            Some("Next-generation ORM for Node.js & TypeScript"),
            vec!["orm", "database", "postgres"],
        ),
        r(
            "facebook/react",
            Some("JavaScript"),
            200_000,
            Some("A JavaScript library for building UIs"),
            vec!["javascript", "ui", "frontend"],
        ),
        r(
            "torvalds/linux",
            Some("C"),
            150_000,
            Some("Linux kernel source tree"),
            vec![],
        ),
        r(
            "rust-lang/rust",
            Some("Rust"),
            90_000,
            Some("Empowering everyone to build reliable software"),
            vec!["language", "rust"],
        ),
        r(
            "denoland/deno",
            Some("Rust"),
            95_000,
            Some("A modern runtime for JavaScript"),
            vec!["javascript", "runtime"],
        ),
        r(
            "tokio-rs/tokio",
            Some("Rust"),
            25_000,
            Some("A runtime for writing reliable async Rust"),
            vec!["rust", "async"],
        ),
    ]
}

#[test]
fn search_postgres_finds_three() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("postgres", None, None, 100, false, false, false, false);
    let names: Vec<&str> = hits.iter().map(|h| h.repo.full_name.as_str()).collect();
    assert!(names.contains(&"postgresml/postgresml"));
    assert!(names.contains(&"supabase/supabase"));
    assert!(names.contains(&"prisma/prisma"));
}

#[test]
fn search_sorts_by_score_then_stars_desc() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("rust", None, None, 100, false, false, false, false);
    for w in hits.windows(2) {
        assert!(
            w[0].score > w[1].score
                || (w[0].score == w[1].score
                    && w[0].repo.stargazers_count >= w[1].repo.stargazers_count),
            "sort order violated between {:?} and {:?}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn search_lang_filter_rust_only() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("", Some("rust"), None, 100, false, false, false, false);
    assert!(!hits.is_empty());
    for h in &hits {
        assert_eq!(h.repo.language.as_deref(), Some("Rust"));
    }
}

#[test]
fn search_lang_filter_is_case_insensitive() {
    let idx = RepoIndex::new(corpus());
    let a = idx.search("", Some("RUST"), None, 100, false, false, false, false);
    let b = idx.search("", Some("rust"), None, 100, false, false, false, false);
    assert_eq!(a.len(), b.len());
}

#[test]
fn search_topic_filter_postgres() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("", None, Some("postgres"), 100, false, false, false, false);
    for h in &hits {
        assert!(h.repo.topics.iter().any(|t| t == "postgres"));
    }
    assert!(hits.len() >= 2);
}

#[test]
fn search_combined_query_and_lang_filter() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("postgres", Some("TypeScript"), None, 100, false, false, false, false);
    for h in &hits {
        assert_eq!(h.repo.language.as_deref(), Some("TypeScript"));
    }
    assert!(!hits.is_empty());
}

#[test]
fn search_limit_truncates_result_set() {
    let idx = RepoIndex::new(corpus());
    let full = idx.search("", None, None, 100, false, false, false, false);
    let cut = idx.search("", None, None, 3, false, false, false, false);
    assert_eq!(full.len(), 10);
    assert_eq!(cut.len(), 3);
    // The top 3 of `cut` should equal the top 3 of `full` (same sort).
    for (a, b) in cut.iter().zip(full.iter().take(3)) {
        assert_eq!(a.repo.full_name, b.repo.full_name);
    }
}

#[test]
fn search_empty_query_matches_all() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("", None, None, 100, false, false, false, false);
    assert_eq!(hits.len(), corpus().len());
}

#[test]
fn search_unknown_term_returns_empty() {
    let idx = RepoIndex::new(corpus());
    let hits = idx.search("definitelynothinglikethis", None, None, 100, false, false, false, false);
    assert!(hits.is_empty());
}

#[test]
fn match_count_equals_unlimited_search_len() {
    let idx = RepoIndex::new(corpus());
    let total = idx.match_count("rust", None, None);
    let all = idx.search("rust", None, None, 1_000_000, false, false, false, false);
    assert_eq!(total, all.len());
}

#[test]
fn cache_grows_on_distinct_queries() {
    let idx = RepoIndex::new(corpus());
    assert_eq!(idx.cache_len(), 0);
    let _ = idx.search("postgres", None, None, 10, false, false, false, false);
    let _ = idx.search("rust", None, None, 10, false, false, false, false);
    let _ = idx.search("javascript", None, None, 10, false, false, false, false);
    assert_eq!(idx.cache_len(), 3);
}

#[test]
fn cache_reuse_does_not_grow_on_repeat() {
    let idx = RepoIndex::new(corpus());
    let _ = idx.search("postgres", None, None, 10, false, false, false, false);
    let _ = idx.search("postgres", None, None, 10, false, false, false, false);
    let _ = idx.search("postgres", None, None, 10, false, false, false, false);
    assert_eq!(idx.cache_len(), 1);
}

#[test]
fn cache_key_considers_lang_filter() {
    let idx = RepoIndex::new(corpus());
    let _ = idx.search("rust", None, None, 10, false, false, false, false);
    let _ = idx.search("rust", Some("Rust"), None, 10, false, false, false, false);
    let _ = idx.search("rust", Some("TypeScript"), None, 10, false, false, false, false);
    assert_eq!(idx.cache_len(), 3);
}

#[test]
fn large_corpus_search_completes_quickly() {
    // 5000 synthetic repos. Mostly a smoke test that rayon + LRU don't
    // blow up on a reasonably sized corpus.
    let mut big = Vec::new();
    for i in 0..5_000 {
        let full = format!("user{}/repo{}", i % 100, i);
        big.push(r(
            &full,
            Some(if i % 3 == 0 { "Rust" } else { "Go" }),
            (i as u64) * 3,
            Some(&format!("synthetic description {}", i)),
            vec!["synthetic"],
        ));
    }
    let idx = RepoIndex::new(big);
    let hits = idx.search("synthetic", None, None, 50, false, false, false, false);
    assert_eq!(hits.len(), 50);
    let rust_hits = idx.search("", Some("Rust"), None, 10_000, false, false, false, false);
    assert!(!rust_hits.is_empty());
    for h in &rust_hits {
        assert_eq!(h.repo.language.as_deref(), Some("Rust"));
    }
}

// OR mode and boost features to be implemented in future release