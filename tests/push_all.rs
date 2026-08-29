use std::fs;
use std::path::{Path, PathBuf};

use agentstack::commands::{PushAllOptions, push_all_with_client};
use agentstack::registry::{MockRegistryClient, Visibility};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn unique_dir(prefix: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agentstack-push-all-{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn make_skill(root: &Path, name: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: Use when working on {name} tasks\n---\n\n# Purpose\n\nDo {name} work.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
        ),
    )
    .unwrap();
}

fn make_invalid_name_mismatch(root: &Path, dir_name: &str, manifest_name: &str) {
    let dir = root.join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {manifest_name}\ndescription: Use when this invalid skill is inspected\n---\n\n# Purpose\n\nMismatch.\n"
        ),
    )
    .unwrap();
}

fn opts<'a>(root: &'a Path, include: Vec<String>, exclude: Vec<String>) -> PushAllOptions<'a> {
    PushAllOptions {
        root,
        org: "acme",
        visibility: Visibility::Private,
        team: None,
        platforms: vec![],
        include,
        exclude,
        dry_run: false,
    }
}

#[test]
fn push_all_walks_directory_and_pushes_all_valid() {
    let root = unique_dir("all-valid");
    make_skill(&root, "alpha");
    make_skill(&root, "beta");
    make_skill(&root, "gamma");

    let mock = MockRegistryClient::new();
    let outcome = push_all_with_client(Some(&mock), None, opts(&root, vec![], vec![])).unwrap();

    assert_eq!(outcome.pushed.len(), 3);
    assert!(outcome.skipped.is_empty());
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 3);
    for name in ["alpha", "beta", "gamma"] {
        assert!(mock.pushed_metadata("acme", name, "1").is_some());
    }
}

#[test]
fn push_all_skips_invalid_skill_and_continues() {
    let root = unique_dir("invalid-continues");
    make_skill(&root, "alpha");
    make_invalid_name_mismatch(&root, "broken", "not-broken");
    make_skill(&root, "gamma");

    let mock = MockRegistryClient::new();
    let outcome = push_all_with_client(Some(&mock), None, opts(&root, vec![], vec![])).unwrap();

    assert_eq!(outcome.pushed.len(), 2);
    assert!(outcome.skipped.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].name, "broken");
    assert_eq!(outcome.failed[0].reason, "name_mismatch");
    assert_eq!(mock.push_count(), 2);
    assert!(mock.pushed_metadata("acme", "alpha", "1").is_some());
    assert!(mock.pushed_metadata("acme", "gamma", "1").is_some());
}

#[test]
fn push_all_with_include_filters() {
    let root = unique_dir("include");
    make_skill(&root, "code-review");
    make_skill(&root, "code-test");
    make_skill(&root, "sql-linter");

    let mock = MockRegistryClient::new();
    let outcome = push_all_with_client(
        Some(&mock),
        None,
        opts(&root, vec!["code-*".into()], vec![]),
    )
    .unwrap();

    assert_eq!(outcome.pushed.len(), 2);
    assert_eq!(outcome.skipped.len(), 1);
    assert_eq!(outcome.skipped[0].name, "sql-linter");
    assert_eq!(outcome.skipped[0].reason, "excluded");
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 2);
}

#[test]
fn push_all_with_exclude_filters() {
    let root = unique_dir("exclude");
    make_skill(&root, "code-review");
    make_skill(&root, "code-test");
    make_skill(&root, "sql-test");

    let mock = MockRegistryClient::new();
    let outcome = push_all_with_client(
        Some(&mock),
        None,
        opts(&root, vec![], vec!["*-test".into()]),
    )
    .unwrap();

    assert_eq!(outcome.pushed.len(), 1);
    assert_eq!(outcome.pushed[0].name, "code-review");
    assert_eq!(outcome.skipped.len(), 2);
    assert!(outcome.skipped.iter().any(|row| row.name == "code-test"));
    assert!(outcome.skipped.iter().any(|row| row.name == "sql-test"));
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 1);
}

#[test]
fn push_all_dry_run_json_uses_batch_shape() {
    let root = unique_dir("json-shape");
    make_skill(&root, "code-review");
    make_skill(&root, "sql-linter");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "--json",
            "skill",
            "push",
            "--all",
            ".",
            "--org",
            "acme",
            "--dry-run",
            "--include",
            "code-*",
        ])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["batch"].as_bool(), Some(true));
    assert_eq!(json["dry_run"].as_bool(), Some(true));
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["pushed"].as_array().unwrap().len(), 1);
    assert_eq!(json["pushed"][0]["name"].as_str(), Some("code-review"));
    assert_eq!(json["pushed"][0]["version"].as_str(), Some("local-dev"));
    assert_eq!(json["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(json["failed"].as_array().unwrap().len(), 0);
    assert_eq!(json["summary"]["pushed"].as_u64(), Some(1));
    assert_eq!(json["summary"]["would_push"].as_u64(), Some(1));
    assert_eq!(json["summary"]["skipped"].as_u64(), Some(1));
    assert_eq!(json["summary"]["failed"].as_u64(), Some(0));
}

#[test]
fn push_all_json_with_no_selected_skips_registry_lookup() {
    let root = unique_dir("json-no-selected");
    make_skill(&root, "code-review");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "--json",
            "skill",
            "push",
            "--all",
            ".",
            "--org",
            "acme",
            "--include",
            "missing-*",
        ])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["batch"].as_bool(), Some(true));
    assert_eq!(json["pushed"].as_array().unwrap().len(), 0);
    assert_eq!(json["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(json["failed"].as_array().unwrap().len(), 0);
    assert_eq!(json["summary"]["pushed"].as_u64(), Some(0));
    assert_eq!(json["summary"]["skipped"].as_u64(), Some(1));
    assert_eq!(json["summary"]["failed"].as_u64(), Some(0));
}

#[test]
fn push_all_human_no_selected_prints_next_action() {
    let root = unique_dir("human-no-selected");
    make_skill(&root, "code-review");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "skill",
            "push",
            "--all",
            ".",
            "--org",
            "acme",
            "--include",
            "missing-*",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("no skills selected to push under"))
        .stdout(predicate::str::contains("next: agentstack skill scan"))
        .stdout(predicate::str::contains("Next command:").not());
}

#[test]
fn push_all_no_input_with_only_invalid_skips_registry_lookup() {
    let root = unique_dir("no-input-invalid-only");
    make_invalid_name_mismatch(&root, "broken", "not-broken");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args(["--no-input", "skill", "push", "--all", ".", "--org", "acme"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("failed 1"))
        .stderr(predicate::str::contains("push --all failed for 1 skill"));
}

#[test]
fn push_all_dry_run_invalid_exits_nonzero_after_summary() {
    let root = unique_dir("invalid-exit");
    make_skill(&root, "alpha");
    make_invalid_name_mismatch(&root, "broken", "not-broken");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "skill",
            "push",
            "--all",
            ".",
            "--org",
            "acme",
            "--dry-run",
            "--yes",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("pushed 1"))
        .stdout(predicate::str::contains("failed 1"))
        .stderr(predicate::str::contains("push --all failed for 1 skill"));
}
