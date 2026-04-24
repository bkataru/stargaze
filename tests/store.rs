//! Integration tests for the redb storage layer.
//!
//! Every test creates its own temp directory so they can run in parallel
//! without interfering. The `TempDir` is held by the caller; when it
//! drops, the directory and the redb file inside it are both deleted.

use chrono::Utc;
use std::collections::HashSet;
use tempfile::TempDir;

use stargaze::{
    count_repos, load_all, load_one, open_db, read_meta, retain_repos, upsert_repos, Repo,
};

fn tmp_db() -> (TempDir, redb::Database) {
    let dir = tempfile::Builder::new()
        .prefix("stargaze-test-")
        .tempdir()
        .expect("tempdir");
    let path = dir.path().join("stars.redb");
    let db = open_db(&path).expect("open_db");
    (dir, db)
}

fn make_repo(name: &str, stars: u64) -> Repo {
    Repo {
        full_name: name.to_string(),
        owner: name.split('/').next().unwrap_or("").to_string(),
        name: name.split('/').nth(1).unwrap_or("").to_string(),
        description: Some(format!("desc of {}", name)),
        url: format!("https://github.com/{}", name),
        language: Some("Rust".into()),
        stargazers_count: stars,
        forks_count: 0,
        open_issues_count: 0,
        topics: vec!["rust".to_string()],
        default_branch: Some("main".into()),
        license: Some("MIT".into()),
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

#[test]
fn roundtrip_upsert_then_load_all() {
    let (_dir, db) = tmp_db();
    let repos = vec![
        make_repo("foo/a", 10),
        make_repo("foo/b", 20),
        make_repo("bar/c", 30),
    ];
    let n = upsert_repos(&db, &repos).unwrap();
    assert_eq!(n, 3);

    let loaded = load_all(&db).unwrap();
    assert_eq!(loaded.len(), 3);
    let names: Vec<&str> = loaded.iter().map(|r| r.full_name.as_str()).collect();
    assert!(names.contains(&"foo/a"));
    assert!(names.contains(&"foo/b"));
    assert!(names.contains(&"bar/c"));
}

#[test]
fn load_all_empty_before_any_write() {
    let (_dir, db) = tmp_db();
    let loaded = load_all(&db).unwrap();
    assert!(loaded.is_empty());
}

#[test]
fn count_repos_matches_load_all_len() {
    let (_dir, db) = tmp_db();
    let repos = vec![
        make_repo("foo/a", 1),
        make_repo("foo/b", 2),
        make_repo("foo/c", 3),
        make_repo("foo/d", 4),
    ];
    upsert_repos(&db, &repos).unwrap();
    assert_eq!(count_repos(&db).unwrap(), 4);
    assert_eq!(load_all(&db).unwrap().len(), 4);
}

#[test]
fn upsert_is_idempotent_on_same_full_name() {
    let (_dir, db) = tmp_db();
    let r = make_repo("foo/a", 10);
    upsert_repos(&db, std::slice::from_ref(&r)).unwrap();
    upsert_repos(&db, std::slice::from_ref(&r)).unwrap();
    upsert_repos(&db, std::slice::from_ref(&r)).unwrap();
    assert_eq!(count_repos(&db).unwrap(), 1);
}

#[test]
fn upsert_overwrites_changed_fields() {
    let (_dir, db) = tmp_db();
    let mut r = make_repo("foo/a", 10);
    upsert_repos(&db, &[r.clone()]).unwrap();

    r.stargazers_count = 999;
    r.description = Some("new desc".into());
    upsert_repos(&db, &[r.clone()]).unwrap();

    let loaded = load_one(&db, "foo/a").unwrap().unwrap();
    assert_eq!(loaded.stargazers_count, 999);
    assert_eq!(loaded.description.as_deref(), Some("new desc"));
}

#[test]
fn load_one_missing_returns_none() {
    let (_dir, db) = tmp_db();
    upsert_repos(&db, &[make_repo("foo/a", 1)]).unwrap();
    assert!(load_one(&db, "nope/nope").unwrap().is_none());
}

#[test]
fn load_one_before_any_write_returns_none() {
    let (_dir, db) = tmp_db();
    assert!(load_one(&db, "foo/a").unwrap().is_none());
}

#[test]
fn meta_tracks_last_sync_time_and_count() {
    let (_dir, db) = tmp_db();
    upsert_repos(&db, &[make_repo("foo/a", 1), make_repo("foo/b", 2)]).unwrap();

    let ts = read_meta(&db, "last_sync").unwrap().unwrap();
    assert!(ts.contains('T'), "expected ISO-8601 timestamp, got {}", ts);

    let count = read_meta(&db, "last_sync_count").unwrap().unwrap();
    assert_eq!(count, "2");
}

#[test]
fn read_meta_missing_key_returns_none() {
    let (_dir, db) = tmp_db();
    assert!(read_meta(&db, "nonexistent").unwrap().is_none());
}

#[test]
fn retain_repos_removes_missing_keys() {
    let (_dir, db) = tmp_db();
    upsert_repos(
        &db,
        &[
            make_repo("foo/a", 1),
            make_repo("foo/b", 2),
            make_repo("foo/c", 3),
        ],
    )
    .unwrap();

    let keep: HashSet<String> = ["foo/a".to_string(), "foo/c".to_string()]
        .into_iter()
        .collect();
    let removed = retain_repos(&db, &keep).unwrap();
    assert_eq!(removed, 1);

    let remaining = load_all(&db).unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|r| r.full_name == "foo/a"));
    assert!(remaining.iter().any(|r| r.full_name == "foo/c"));
    assert!(remaining.iter().all(|r| r.full_name != "foo/b"));
}

#[test]
fn retain_repos_noop_when_all_kept() {
    let (_dir, db) = tmp_db();
    upsert_repos(&db, &[make_repo("foo/a", 1), make_repo("foo/b", 2)]).unwrap();
    let keep: HashSet<String> = ["foo/a".to_string(), "foo/b".to_string()]
        .into_iter()
        .collect();
    assert_eq!(retain_repos(&db, &keep).unwrap(), 0);
    assert_eq!(count_repos(&db).unwrap(), 2);
}

#[test]
fn retain_repos_empty_table_is_zero() {
    let (_dir, db) = tmp_db();
    let keep: HashSet<String> = HashSet::new();
    assert_eq!(retain_repos(&db, &keep).unwrap(), 0);
}

#[test]
fn many_repos_roundtrip() {
    let (_dir, db) = tmp_db();
    let repos: Vec<Repo> = (0..500)
        .map(|i| make_repo(&format!("user/repo{}", i), i as u64))
        .collect();
    upsert_repos(&db, &repos).unwrap();
    assert_eq!(count_repos(&db).unwrap(), 500);
    let loaded = load_all(&db).unwrap();
    assert_eq!(loaded.len(), 500);
}
