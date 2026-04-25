//! Integration tests for the CLI binary.
//! These tests spawn the `stargaze` binary and test CLI commands.

use std::process::Command;
use tempfile::TempDir;

fn binary_path() -> String {
    // Use cargo to get the binary path
    let output = Command::new("cargo")
        .args(&["build", "--message-format=json"])
        .current_dir("/home/int-wizard/stargaze")
        .output()
        .expect("Failed to run cargo build");
    
    // Parse the JSON output to find the binary path
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("\"executable\"") {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(exe) = json.get("executable").and_then(|v| v.as_str()) {
                    return exe.to_string();
                }
            }
        }
    }
    
    // Fallback: just use the debug binary path
    "/home/int-wizard/stargaze/target/debug/stargaze".to_string()
}

#[test]
fn cli_help_exits_zero() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .arg("--help")
        .output()
        .expect("Failed to run stargaze");
    assert!(output.status.success(), "Expected --help to exit 0, got {}", output.status);
}

#[test]
fn cli_version_exits_zero() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .arg("--version")
        .output()
        .expect("Failed to run stargaze");
    assert!(output.status.success(), "Expected --version to exit 0, got {}", output.status);
}

#[test]
fn cli_search_help() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(&["search", "--help"])
        .output()
        .expect("Failed to run stargaze search --help");
    assert!(output.status.success(), "Expected search --help to exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("search"), "Expected help to mention 'search'");
}

#[test]
fn cli_show_help() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(&["show", "--help"])
        .output()
        .expect("Failed to run stargaze show --help");
    assert!(output.status.success());
}

#[test]
fn cli_stats_help() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(&["stats", "--help"])
        .output()
        .expect("Failed to run stargaze stats --help");
    assert!(output.status.success());
}

#[test]
fn cli_list_help() {
    let bin = binary_path();
    let output = Command::new(&bin)
        .args(&["list", "--help"])
        .output()
        .expect("Failed to run stargaze list --help");
    assert!(output.status.success());
}

#[test]
fn cli_with_test_db_search_empty() {
    let bin = binary_path();
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.redb");
    
    // Search on empty database should work (return 0 results)
    let output = Command::new(&bin)
        .args(&["--db", db_path.to_str().unwrap(), "search", "test"])
        .output()
        .expect("Failed to run stargaze search");
    // Should exit 0 (no error, just 0 results)
    assert!(output.status.success(), "Expected search to exit 0, got {}", output.status);
}

#[test]
fn cli_with_test_db_stats_empty() {
    let bin = binary_path();
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.redb");
    
    let output = Command::new(&bin)
        .args(&["--db", db_path.to_str().unwrap(), "stats"])
        .output()
        .expect("Failed to run stargaze stats");
    assert!(output.status.success(), "Expected stats to exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0"), "Expected stats to show 0 repos");
}
