use agentstack::skill::{LintConfig, lint_skill, validate_skill};
use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;

#[test]
fn init_creates_expected_layout() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("my-skill");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args([
            "skill",
            "init",
            "my-skill",
            "--name",
            "my-skill",
            "--description",
            "triggers when foo",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("created skill `my-skill`"))
        .stdout(predicate::str::contains("path:"))
        .stdout(predicate::str::contains("skill.md:"))
        .stdout(predicate::str::contains("next:"));

    target.child("SKILL.md").assert(predicate::path::is_file());
    target.child("references").assert(predicate::path::is_dir());
    target.child("examples").assert(predicate::path::is_dir());
    target.child("assets").assert(predicate::path::is_dir());
    target.child("platform").assert(predicate::path::is_dir());

    let skill_md = std::fs::read_to_string(target.child("SKILL.md").path()).unwrap();
    assert!(skill_md.contains("name: my-skill"));
    assert!(skill_md.contains("description: triggers when foo"));
    for section in [
        "# Purpose",
        "# When to Use",
        "# Instructions",
        "# Output",
        "# Boundaries",
    ] {
        assert!(skill_md.contains(section), "missing section {section}");
    }
    assert!(!skill_md.contains("TODO:"));
    assert!(
        skill_md
            .contains("Help an agent respond well when this trigger applies: triggers when foo.")
    );
    assert!(skill_md.contains("Use when triggers when foo."));
}

#[test]
fn init_defaults_path_to_name() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args([
            "skill",
            "init",
            "--name",
            "another-skill",
            "--description",
            "does X",
        ])
        .assert()
        .success();

    tmp.child("another-skill/SKILL.md")
        .assert(predicate::path::is_file());
}

#[test]
fn init_quiet_creates_skill_without_next_steps() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("quiet-skill");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args([
            "--quiet",
            "skill",
            "init",
            "quiet-skill",
            "--name",
            "quiet-skill",
            "--description",
            "stays quiet",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    target.child("SKILL.md").assert(predicate::path::is_file());
}

#[test]
fn init_rejects_invalid_name() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args(["skill", "init", "--name", "Bad_Name", "--description", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --name"));
}

#[test]
fn init_refuses_non_empty_directory() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("occupied");
    target.create_dir_all().unwrap();
    target.child("notes.txt").write_str("hi").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args([
            "skill",
            "init",
            "occupied",
            "--name",
            "occupied",
            "--description",
            "x",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("non-empty directory"));
}

#[test]
fn init_then_validate_and_lint_passes_without_placeholders() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("round-trip");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args([
            "skill",
            "init",
            "--name",
            "round-trip",
            "--description",
            "Use when verifying that fresh init scaffolds pass checks.",
        ])
        .assert()
        .success();

    let outcome = validate_skill(target.path());
    assert!(
        outcome.is_ok(),
        "fresh init scaffold did not validate: {:?}",
        outcome.errors
    );

    let parsed = outcome
        .parsed
        .as_ref()
        .expect("validated skill should include parsed SKILL.md");
    let content = outcome
        .content
        .as_deref()
        .expect("validated skill should include raw SKILL.md content");
    let warnings = lint_skill(target.path(), parsed, content, &LintConfig::default());
    assert!(
        warnings.is_empty(),
        "fresh init scaffold should be lint-clean: {warnings:?}"
    );
}
