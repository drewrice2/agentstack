use std::fs;
use std::path::{Path, PathBuf};

use agentstack::commands::{AdoptOptions, adopt_with_client};
use agentstack::registry::{MockRegistryClient, Visibility};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

fn unique_dir(prefix: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agentstack-adopt-{prefix}-{}-{}",
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

fn opts(root: &Path, dry_run: bool) -> AdoptOptions<'_> {
    AdoptOptions {
        root,
        org: "acme",
        visibility: Visibility::Private,
        team: None,
        platforms: vec![],
        dry_run,
    }
}

#[test]
fn adopt_pushes_every_valid_skill_as_candidate() {
    let root = unique_dir("multi");
    make_skill(&root, "alpha");
    make_skill(&root, "beta");
    make_skill(&root, "gamma");

    let mock = MockRegistryClient::new();
    let outcome = adopt_with_client(Some(&mock), None, opts(&root, false)).unwrap();

    assert_eq!(outcome.adopted.len(), 3);
    assert!(outcome.skipped.is_empty());
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 3);
    for name in ["alpha", "beta", "gamma"] {
        assert!(mock.pushed_metadata("acme", name, "1").is_some());
    }
    let alpha = &outcome.adopted[0];
    assert_eq!(alpha.name, "alpha");
    assert_eq!(alpha.skill_ref, "acme/alpha@1");
    assert_eq!(alpha.version, "1");
    assert!(alpha.audit_event_id.is_some());
}

#[test]
fn adopt_skips_invalid_skill_and_continues() {
    let root = unique_dir("invalid-skipped");
    make_skill(&root, "alpha");
    make_invalid_name_mismatch(&root, "broken", "not-broken");
    make_skill(&root, "gamma");

    let mock = MockRegistryClient::new();
    let outcome = adopt_with_client(Some(&mock), None, opts(&root, false)).unwrap();

    assert_eq!(outcome.adopted.len(), 2);
    assert_eq!(outcome.skipped.len(), 1);
    assert!(outcome.skipped[0].path.ends_with("broken"));
    assert!(outcome.skipped[0].reason.contains("name_mismatch"));
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 2);
    assert!(mock.pushed_metadata("acme", "alpha", "1").is_some());
    assert!(mock.pushed_metadata("acme", "gamma", "1").is_some());
}

#[test]
fn adopt_dry_run_uploads_nothing() {
    let root = unique_dir("dry-run");
    make_skill(&root, "alpha");
    make_skill(&root, "beta");

    let mock = MockRegistryClient::new();
    let outcome = adopt_with_client(Some(&mock), None, opts(&root, true)).unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.adopted.len(), 2);
    assert!(outcome.failed.is_empty());
    assert_eq!(mock.push_count(), 0);
    assert!(
        outcome
            .adopted
            .iter()
            .all(|row| row.audit_event_id.is_none())
    );
}

#[test]
fn adopt_partial_push_failure_does_not_abort_batch() {
    let root = unique_dir("partial-failure");
    make_skill(&root, "alpha");
    make_skill(&root, "beta");

    let mock = MockRegistryClient::new();
    mock.fail_next_push("permission denied for `acme/alpha`");
    let outcome = adopt_with_client(Some(&mock), None, opts(&root, false)).unwrap();

    assert_eq!(outcome.adopted.len(), 1);
    assert_eq!(outcome.adopted[0].name, "beta");
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].name, "alpha");
    assert!(outcome.failed[0].reason.contains("permission denied"));
    assert_eq!(mock.push_count(), 2);
}

#[test]
fn adopt_dry_run_json_uses_adopt_shape() {
    let root = unique_dir("json-shape");
    make_skill(&root, "code-review");
    make_invalid_name_mismatch(&root, "broken", "not-broken");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "--json",
            "skill",
            "adopt",
            ".",
            "--org",
            "acme",
            "--dry-run",
        ])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["dry_run"].as_bool(), Some(true));
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["adopted"].as_array().unwrap().len(), 1);
    assert_eq!(json["adopted"][0]["name"].as_str(), Some("code-review"));
    assert_eq!(json["adopted"][0]["version"].as_str(), Some("local-dev"));
    assert!(json["adopted"][0]["audit_event_id"].is_null());
    assert_eq!(json["skipped"].as_array().unwrap().len(), 1);
    assert!(
        json["skipped"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("broken")
    );
    assert!(
        json["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("name_mismatch")
    );
    assert_eq!(json["failed"].as_array().unwrap().len(), 0);
    assert_eq!(json["summary"]["adopted"].as_u64(), Some(1));
    assert_eq!(json["summary"]["would_adopt"].as_u64(), Some(1));
    assert_eq!(json["summary"]["skipped"].as_u64(), Some(1));
    assert_eq!(json["summary"]["failed"].as_u64(), Some(0));
}

#[test]
fn adopt_dry_run_with_invalid_skill_exits_zero() {
    let root = unique_dir("invalid-exit-zero");
    make_skill(&root, "alpha");
    make_invalid_name_mismatch(&root, "broken", "not-broken");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args(["skill", "adopt", ".", "--org", "acme", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("plan: 1 to push, 1 skipped"))
        .stdout(predicate::str::contains("adopted 1 · skipped 1 · failed 0"))
        .stdout(predicate::str::contains("dry run; nothing uploaded"));
}

#[test]
fn adopt_json_empty_directory_reports_empty_message() {
    let root = unique_dir("json-empty");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args([
            "--json",
            "skill",
            "adopt",
            ".",
            "--org",
            "acme",
            "--dry-run",
        ])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["adopted"].as_array().unwrap().len(), 0);
    assert_eq!(json["skipped"].as_array().unwrap().len(), 0);
    assert_eq!(json["failed"].as_array().unwrap().len(), 0);
    assert!(
        json["empty_message"]
            .as_str()
            .unwrap()
            .contains("no skills found under")
    );
}

#[test]
fn adopt_json_without_yes_refuses_to_prompt() {
    let root = unique_dir("json-no-yes");
    make_skill(&root, "alpha");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(&root)
        .args(["--json", "skill", "adopt", ".", "--org", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--yes"));
}

#[cfg(unix)]
#[test]
fn adopt_skips_symlinked_skill_dirs() {
    let root = unique_dir("symlink-skip");
    make_skill(&root, "alpha");
    let outside = unique_dir("symlink-outside");
    make_skill(&outside, "outside-skill");
    std::os::unix::fs::symlink(outside.join("outside-skill"), root.join("outside-skill")).unwrap();

    let mock = MockRegistryClient::new();
    let outcome = adopt_with_client(Some(&mock), None, opts(&root, false)).unwrap();

    assert_eq!(outcome.adopted.len(), 1, "{outcome:?}");
    assert_eq!(outcome.adopted[0].name, "alpha");
    assert_eq!(outcome.skipped.len(), 1, "{outcome:?}");
    assert!(
        outcome.skipped[0]
            .reason
            .contains("resolves outside the adopt root"),
        "reason: {}",
        outcome.skipped[0].reason
    );
    assert_eq!(mock.push_count(), 1, "the symlinked skill must not upload");
}
