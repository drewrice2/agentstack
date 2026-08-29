use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn write_skill(dir: &ChildPath, name: &str, description: &str) {
    dir.create_dir_all().unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    dir.child("SKILL.md").write_str(&body).unwrap();
}

#[test]
fn list_local_finds_subdirectory_skills() {
    let tmp = TempDir::new().unwrap();
    write_skill(&tmp.child("alpha"), "alpha", "first");
    write_skill(&tmp.child("beta"), "beta", "second");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args(["skill", "scan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("first"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("second"));
}

#[test]
fn skill_scan_accepts_explicit_path() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.child("skills");
    write_skill(&root.child("alpha"), "alpha", "first");

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args(["skill", "scan"])
        .arg(root.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("first"));
}

#[test]
fn list_local_handles_empty_tree() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .args(["skill", "scan"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no skills found"))
        .stdout(predicate::str::contains("next: agentstack skill init"));
}

#[test]
fn root_list_is_not_supported() {
    let tmp = TempDir::new().unwrap();
    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .arg("list")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand 'list'"));
}
