use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn write_full_skill(dir: &ChildPath) {
    dir.create_dir_all().unwrap();
    let name = dir.path().file_name().unwrap().to_str().unwrap();
    dir.child("SKILL.md")
        .write_str(&format!(
            "---\nname: {name}\ndescription: Use when foo happens, the agent should run this skill\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
        ))
        .unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        dir.child(sub).create_dir_all().unwrap();
    }
}

#[test]
fn lint_passes_clean_skill() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("clean");
    write_full_skill(&target);

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("0 warnings"));
}

#[test]
fn lint_warns_on_missing_sections() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-sections");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str("---\nname: no-sections\ndescription: Use when foo\n---\n")
        .unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        target.child(sub).create_dir_all().unwrap();
    }

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[missing_section_purpose]"))
        .stdout(predicate::str::contains("[missing_section_when_to_use]"))
        .stdout(predicate::str::contains("[missing_section_instructions]"))
        .stdout(predicate::str::contains("[missing_section_output]"))
        .stdout(predicate::str::contains("[missing_section_boundaries]"));
}

#[test]
fn lint_warns_on_missing_directories() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-dirs");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str(
            "---\nname: no-dirs\ndescription: Use when foo happens, the agent should run this skill\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[no_examples_directory]"))
        .stdout(predicate::str::contains("[no_references_directory]"));
}

#[test]
fn lint_warns_on_non_trigger_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("no-trigger");
    write_full_skill(&target);
    target
        .child("SKILL.md")
        .write_str(
            "---\nname: no-trigger\ndescription: Refactors a module that should be split apart\n---\n\n# Purpose\n# When to Use\n# Instructions\n# Output\n# Boundaries\n",
        )
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[non_trigger_description]"));
}

#[test]
fn lint_warns_on_vague_description() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("vague");
    write_full_skill(&target);
    target
        .child("SKILL.md")
        .write_str(
            "---\nname: vague\ndescription: Use stuff\n---\n\n# Purpose\n# When to Use\n# Instructions\n# Output\n# Boundaries\n",
        )
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[vague_description]"));
}

#[test]
fn lint_warns_on_placeholder_todos() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("placeholder");
    write_full_skill(&target);
    target
        .child("SKILL.md")
        .write_str(
            "---\nname: placeholder\ndescription: Use when placeholder checks run\n---\n\n# Purpose\n\nTODO: Replace this.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[placeholder_content]"));
}

#[test]
fn lint_warns_on_skill_md_too_long() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("too-long");
    write_full_skill(&target);
    let big_body = "lorem ipsum dolor sit amet ".repeat(80);
    target
        .child("SKILL.md")
        .write_str(&format!(
            "---\nname: too-long\ndescription: Use when this fires\n---\n\n# Purpose\n\n{big_body}\n\n# When to Use\n# Instructions\n# Output\n# Boundaries\n",
        ))
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint", "--max-chars", "200"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[skill_md_too_long]"));
}

#[test]
fn lint_warns_on_unreferenced_reference() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("unref");
    write_full_skill(&target);
    target
        .child("references/api.md")
        .write_str("# API\n")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("[unreferenced_reference]"))
        .stdout(predicate::str::contains("api.md"));
}

#[test]
fn lint_does_not_warn_when_reference_is_mentioned() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("ref");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str(
            "---\nname: ref\ndescription: Use when foo happens repeatedly\n---\n\n# Purpose\nSee references/api.md\n\n# When to Use\n# Instructions\n# Output\n# Boundaries\n",
        )
        .unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        target.child(sub).create_dir_all().unwrap();
    }
    target
        .child("references/api.md")
        .write_str("# API\n")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .success();
}

#[test]
fn lint_fails_when_validation_fails() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("invalid");
    target.create_dir_all().unwrap();
    // Missing SKILL.md → validation error → lint cannot run.

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "lint"])
        .arg(target.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains("[missing_skill_md]"));
}
