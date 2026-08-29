use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

const FULL_SKILL_MD: &str = "\
---
name: my-skill
description: Use when foo happens
---

# Purpose

# When to Use

# Instructions

# Output

# Boundaries
";

fn write_skill(dir: &ChildPath, content: &str) {
    dir.create_dir_all().unwrap();
    dir.child("SKILL.md").write_str(content).unwrap();
}

#[test]
fn valid_minimal_skill_passes() {
    // Frontmatter only — no sections, no subdirs. Validation cares only
    // about the hard rules; sections and subdirs are lint concerns.
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("my-skill");
    write_skill(
        &target,
        "---\nname: my-skill\ndescription: Use when foo\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok ("));
}

#[test]
fn validate_passes_for_full_skill() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("my-skill");
    write_skill(&target, FULL_SKILL_MD);
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        target.child(sub).create_dir_all().unwrap();
    }

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok ("));
}

#[test]
fn validate_passes_when_sections_missing() {
    // Sections are a lint concern, not a validation concern.
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("ok");
    write_skill(
        &target,
        "---\nname: ok\ndescription: Use when nothing\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success();
}

#[test]
fn validate_accepts_root_support_file() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("extra-file");
    write_skill(
        &target,
        "---\nname: extra-file\ndescription: Use when foo\n---\n",
    );
    target.child("README.md").write_str("extra").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok ("));
}

#[test]
fn validate_accepts_scripts_directory() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("scripted-skill");
    write_skill(
        &target,
        "---\nname: scripted-skill\ndescription: Use when helper scripts are needed\n---\n",
    );
    target.child("scripts").create_dir_all().unwrap();
    target
        .child("scripts")
        .child("helper.py")
        .write_str("print('ok')\n")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success();
}

#[test]
fn validate_accepts_arbitrary_support_directory() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("extra-dir");
    write_skill(
        &target,
        "---\nname: extra-dir\ndescription: Use when foo\n---\n",
    );
    target.child("templates").create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok ("));
}

#[test]
fn validate_reports_missing_skill_md() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("empty");
    target.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[missing_skill_md]"));
}

#[test]
fn validate_reports_missing_directory() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.child("does-not-exist");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(missing.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[not_a_directory]"));
}

#[test]
fn validate_json_failure_emits_only_error_envelope() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.child("does-not-exist");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["--json", "skill", "validate"])
        .arg(missing.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());

    let json: Value = serde_json::from_slice(&assert.get_output().stderr)
        .expect("stderr should be a single JSON error envelope");
    assert_eq!(json["error"]["code"], "not_a_directory");
    assert_eq!(json["error"]["action"], "validate_skill");
}

#[test]
fn validate_reports_invalid_frontmatter() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("bad-yaml");
    write_skill(&target, "---\nname: : :\n---\n");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[invalid_frontmatter]"));
}

#[test]
fn validate_reports_missing_frontmatter() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-frontmatter");
    write_skill(&target, "# Purpose\n");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[missing_frontmatter]"));
}

#[test]
fn validate_reports_invalid_slug() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("bad-slug");
    write_skill(
        &target,
        "---\nname: Bad_Name\ndescription: Use when foo\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[invalid_name]"));
}

#[test]
fn validate_reports_name_directory_mismatch() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("actual-name");
    write_skill(
        &target,
        "---\nname: other-name\ndescription: Use when foo\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[name_mismatch]"))
        .stderr(predicate::str::contains("actual-name"));
}

#[test]
fn human_output_includes_file_line_col() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("actual-name");
    write_skill(
        &target,
        "---\nname: other-name\ndescription: Use when foo\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "SKILL.md:2:1: error[name_mismatch]:",
        ));
}

#[test]
fn validate_reports_missing_name() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-name");
    write_skill(&target, "---\ndescription: Use when foo\n---\n");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[missing_name]"));
}

#[test]
fn validate_reports_missing_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-desc");
    write_skill(&target, "---\nname: ok\n---\n");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[missing_description]"));
}

#[test]
fn validate_reports_too_long_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("long-desc");
    let huge = "a".repeat(501);
    write_skill(
        &target,
        &format!("---\nname: ok\ndescription: {huge}\n---\n"),
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[description_too_long]"));
}

#[test]
fn validate_reports_multiline_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("multi-desc");
    write_skill(
        &target,
        "---\nname: multi-desc\ndescription: |-\n  line one\n  line two\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "SKILL.md:3:1: error[description_multiline]:",
        ));
}

#[test]
fn validate_json_reports_multiline_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("multi-desc");
    write_skill(
        &target,
        "---\nname: multi-desc\ndescription: |-\n  line one\n  line two\n---\n",
    );

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["--json", "skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());

    let json: Value = serde_json::from_slice(&assert.get_output().stderr)
        .expect("stderr should be a single JSON error envelope");
    assert_eq!(json["error"]["code"], "description_multiline");
    assert_eq!(json["error"]["action"], "validate_skill");
}

#[test]
fn validate_accepts_folded_single_line_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("folded-desc");
    write_skill(
        &target,
        "---\nname: folded-desc\ndescription: >-\n  Use when foo\n  and bar\n---\n",
    );

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ok ("));
}

#[test]
fn validate_reports_invalid_utf8() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("non-utf8");
    target.create_dir_all().unwrap();
    // Bytes that are not valid UTF-8 (lone continuation byte).
    std::fs::write(target.child("SKILL.md").path(), [0xff, 0xfe, 0x80]).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "validate"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[invalid_utf8]"));
}
