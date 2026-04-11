//! Integration tests for the parsing layer.

use stargaze::{parse_link_next, Repo};

#[test]
fn link_next_with_prev_next_last() {
    let h = r#"<https://api.github.com/user/starred?page=1>; rel="prev", <https://api.github.com/user/starred?page=3>; rel="next", <https://api.github.com/user/starred?page=5>; rel="last""#;
    assert_eq!(
        parse_link_next(h).unwrap(),
        "https://api.github.com/user/starred?page=3"
    );
}

#[test]
fn link_next_absent_when_last_only() {
    let h = r#"<https://api.github.com/user/starred?page=1>; rel="last""#;
    assert!(parse_link_next(h).is_none());
}

#[test]
fn link_next_empty_string() {
    assert!(parse_link_next("").is_none());
}

#[test]
fn repo_parses_minimal_payload() {
    let v = serde_json::json!({
        "full_name": "x/y",
        "name": "y",
        "html_url": "",
    });
    let r = Repo::from_api(&v).unwrap();
    assert_eq!(r.full_name, "x/y");
    assert_eq!(r.owner, "x");
    assert_eq!(r.stargazers_count, 0);
}

#[test]
fn repo_parses_star_wrapper_with_full_payload() {
    let v = serde_json::json!({
        "starred_at": "2024-01-15T12:34:56Z",
        "repo": {
            "full_name": "foo/bar",
            "name": "bar",
            "owner": {"login": "foo"},
            "description": "a real repo",
            "html_url": "https://github.com/foo/bar",
            "language": "Python",
            "stargazers_count": 5,
            "forks_count": 1,
            "open_issues_count": 2,
            "topics": ["machine-learning", "python"],
            "license": {"spdx_id": "Apache-2.0", "name": "Apache License 2.0"},
            "default_branch": "main",
            "archived": false,
            "fork": false,
            "pushed_at": "2024-03-14T10:20:30Z",
            "created_at": "2020-06-01T00:00:00Z",
            "updated_at": "2024-03-15T00:00:00Z",
        }
    });
    let r = Repo::from_api(&v).unwrap();
    assert_eq!(r.full_name, "foo/bar");
    assert_eq!(r.owner, "foo");
    assert_eq!(r.language.as_deref(), Some("Python"));
    assert_eq!(r.topics.len(), 2);
    assert_eq!(r.license.as_deref(), Some("Apache-2.0"));
    assert!(r.starred_at.is_some());
    assert!(r.pushed_at.is_some());
    assert!(r.created_at.is_some());
    assert!(r.updated_at.is_some());
}

#[test]
fn repo_rejects_missing_full_name() {
    let v = serde_json::json!({"name": "bar"});
    assert!(Repo::from_api(&v).is_err());
}

#[test]
fn repo_rejects_missing_name() {
    let v = serde_json::json!({"full_name": "foo/bar"});
    assert!(Repo::from_api(&v).is_err());
}

#[test]
fn repo_invalid_starred_at_is_none() {
    let v = serde_json::json!({
        "starred_at": "not-a-date",
        "repo": {
            "full_name": "foo/bar",
            "name": "bar",
            "html_url": "",
        }
    });
    let r = Repo::from_api(&v).unwrap();
    assert!(r.starred_at.is_none());
}

#[test]
fn repo_from_api_many_realistic_payloads() {
    let payloads = [
        serde_json::json!({
            "full_name": "rust-lang/rust",
            "name": "rust",
            "owner": {"login": "rust-lang"},
            "description": "Empowering everyone to build reliable and efficient software.",
            "html_url": "https://github.com/rust-lang/rust",
            "stargazers_count": 90000,
            "language": "Rust",
            "topics": ["compiler", "language", "rust"],
            "archived": false,
            "fork": false,
        }),
        serde_json::json!({
            "full_name": "facebook/react",
            "name": "react",
            "owner": {"login": "facebook"},
            "html_url": "https://github.com/facebook/react",
            "stargazers_count": 200000,
            "language": "JavaScript",
            "topics": ["javascript", "ui"],
            "archived": false,
            "fork": false,
        }),
    ];
    let parsed: Vec<Repo> = payloads
        .iter()
        .map(|p| Repo::from_api(p).unwrap())
        .collect();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].full_name, "rust-lang/rust");
    assert_eq!(parsed[1].owner, "facebook");
}
