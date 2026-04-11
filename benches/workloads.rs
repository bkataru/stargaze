//! Criterion benchmarks for stargaze's hot paths.
//!
//! Run with `cargo bench` (uses the default config) or
//! `cargo bench -- --save-baseline <name>` to snapshot a baseline.
//!
//! The synthetic corpus matches the real author's star count (~3k) so
//! numbers are directly comparable to live behavior.

use chrono::Utc;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use stargaze::{parse_link_next, IndexedRepo, Repo, RepoIndex};

fn synth_repo(i: usize) -> Repo {
    let lang = match i % 5 {
        0 => Some("Rust".to_string()),
        1 => Some("TypeScript".to_string()),
        2 => Some("Python".to_string()),
        3 => Some("Go".to_string()),
        _ => None,
    };
    let topics = vec![
        format!("topic{}", i % 20),
        format!("cat{}", i % 7),
        "general".to_string(),
    ];
    Repo {
        full_name: format!("user{}/repo{}", i % 500, i),
        owner: format!("user{}", i % 500),
        name: format!("repo{}", i),
        description: Some(format!(
            "Synthetic repo {} for criterion benchmarks — contains the word postgres occasionally",
            i
        )),
        url: format!("https://github.com/user{}/repo{}", i % 500, i),
        language: lang,
        stargazers_count: (i as u64) * 3 + 1,
        forks_count: i as u64,
        open_issues_count: (i as u64) % 17,
        topics,
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

fn synth_corpus(n: usize) -> Vec<Repo> {
    (0..n).map(synth_repo).collect()
}

fn bench_index_build(c: &mut Criterion) {
    let mut g = c.benchmark_group("index_build");
    for &n in &[100_usize, 1_000, 3_000, 10_000] {
        g.throughput(Throughput::Elements(n as u64));
        let corpus = synth_corpus(n);
        g.bench_with_input(BenchmarkId::from_parameter(n), &corpus, |b, c| {
            b.iter(|| {
                let idx = RepoIndex::new(black_box(c.clone()));
                black_box(idx.len());
            });
        });
    }
    g.finish();
}

fn bench_search_cold(c: &mut Criterion) {
    let mut g = c.benchmark_group("search_cold");
    for &n in &[100_usize, 1_000, 3_000, 10_000] {
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || RepoIndex::new(synth_corpus(n)),
                |idx| {
                    // Rebuild index each iteration so the LRU cache is empty.
                    let hits = idx.search(black_box("postgres"), None, None, 30);
                    black_box(hits.len());
                },
                criterion::BatchSize::LargeInput,
            );
        });
    }
    g.finish();
}

fn bench_search_warm(c: &mut Criterion) {
    let mut g = c.benchmark_group("search_warm");
    let idx = RepoIndex::new(synth_corpus(3_000));
    // Prime the LRU cache once.
    let _ = idx.search("postgres", None, None, 30);
    g.bench_function("same_query_3k", |b| {
        b.iter(|| {
            let hits = idx.search(black_box("postgres"), None, None, 30);
            black_box(hits.len());
        });
    });
    g.finish();
}

fn bench_search_with_filters(c: &mut Criterion) {
    let mut g = c.benchmark_group("search_with_filters");
    let idx = RepoIndex::new(synth_corpus(3_000));
    g.bench_function("lang_rust_3k", |b| {
        b.iter(|| {
            let hits = idx.search("", Some("Rust"), None, 30);
            black_box(hits.len());
        });
    });
    g.bench_function("topic_general_3k", |b| {
        b.iter(|| {
            let hits = idx.search("", None, Some("general"), 30);
            black_box(hits.len());
        });
    });
    g.bench_function("query_lang_topic_3k", |b| {
        b.iter(|| {
            let hits = idx.search("postgres", Some("Rust"), Some("general"), 30);
            black_box(hits.len());
        });
    });
    g.finish();
}

fn bench_indexed_repo_score(c: &mut Criterion) {
    let r = synth_repo(42);
    let ir = IndexedRepo::new(r);
    c.bench_function("indexed_repo_score", |b| {
        b.iter(|| black_box(ir.score(black_box("postgres"))));
    });
}

fn bench_parse_link_next(c: &mut Criterion) {
    let header = r#"<https://api.github.com/user/starred?page=1>; rel="prev", <https://api.github.com/user/starred?page=3>; rel="next", <https://api.github.com/user/starred?page=34>; rel="last", <https://api.github.com/user/starred?page=1>; rel="first""#;
    c.bench_function("parse_link_next", |b| {
        b.iter(|| black_box(parse_link_next(black_box(header))));
    });
}

fn bench_repo_from_api(c: &mut Criterion) {
    let payload = serde_json::json!({
        "full_name": "rust-lang/rust",
        "name": "rust",
        "owner": {"login": "rust-lang"},
        "description": "Empowering everyone to build reliable and efficient software.",
        "html_url": "https://github.com/rust-lang/rust",
        "stargazers_count": 90000,
        "forks_count": 10000,
        "open_issues_count": 5000,
        "language": "Rust",
        "topics": ["compiler", "language", "rust"],
        "default_branch": "master",
        "archived": false,
        "fork": false,
        "pushed_at": "2024-03-14T10:20:30Z",
        "created_at": "2010-06-01T00:00:00Z",
        "updated_at": "2024-03-15T00:00:00Z",
        "license": {"spdx_id": "Apache-2.0"},
    });
    c.bench_function("repo_from_api", |b| {
        b.iter(|| black_box(Repo::from_api(black_box(&payload)).unwrap()));
    });
}

criterion_group!(
    benches,
    bench_index_build,
    bench_search_cold,
    bench_search_warm,
    bench_search_with_filters,
    bench_indexed_repo_score,
    bench_parse_link_next,
    bench_repo_from_api,
);
criterion_main!(benches);
