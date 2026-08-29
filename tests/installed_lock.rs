use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use agentstack::install::{TARGET_INSTALL_LOCK_METADATA, TargetLockMetadata};

fn make_skill(parent: &TempDir, name: &str) -> assert_fs::fixture::ChildPath {
    let skill = parent.child(name);
    skill.create_dir_all().unwrap();
    skill
        .child("SKILL.md")
        .write_str(&format!(
            "---\nname: {name}\ndescription: Use when {name} is needed\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
        ))
        .unwrap();
    skill
}

fn cmd(cfg: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("agentstack").unwrap();
    command.env("AGENTSTACK_CONFIG_DIR", cfg);
    command
}

fn set_local_target(cfg: &std::path::Path, target_root: &std::path::Path) {
    cmd(cfg)
        .args(["target", "set", "local", "--path"])
        .arg(target_root)
        .assert()
        .success();
}

fn write_lock(target_root: &std::path::Path, age_hours: i64) {
    std::fs::create_dir_all(target_root).unwrap();
    let lock = target_root.join(".agentstack-install.lock");
    std::fs::create_dir_all(&lock).unwrap();
    let mut metadata =
        TargetLockMetadata::new(&target_root.canonicalize().unwrap(), Some("install")).unwrap();
    metadata.pid = 4242;
    metadata.hostname = Some("ci-host".to_string());
    metadata.created_at = (OffsetDateTime::now_utc() - time::Duration::hours(age_hours))
        .format(&Rfc3339)
        .unwrap();
    std::fs::write(
        lock.join(TARGET_INSTALL_LOCK_METADATA),
        serde_json::to_string_pretty(&metadata).unwrap(),
    )
    .unwrap();
}

#[test]
fn target_busy_json_includes_safe_lock_context() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    write_lock(target_root.path(), 1);
    let skill = make_skill(&tmp, "busy-skill");

    let output = cmd(cfg.path())
        .env("AGENTSTACK_TARGET_LOCK_TIMEOUT_MS", "1")
        .args(["--json", "skill", "install"])
        .arg(skill.path())
        .args(["--target", "local"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    let error = &json["error"];

    assert!(error["causes"][0].as_str().unwrap().contains("target_busy"));
    assert!(
        error["lock"]["target_path"]
            .as_str()
            .unwrap()
            .ends_with("/target")
    );
    assert!(
        error["lock"]["lock_path"]
            .as_str()
            .unwrap()
            .ends_with(".agentstack-install.lock")
    );
    assert_eq!(error["lock"]["pid"].as_u64(), Some(4242));
    assert_eq!(error["lock"]["hostname"].as_str(), Some("ci-host"));
    assert!(error["lock"]["lock_age_seconds"].as_u64().unwrap() > 0);
    assert!(
        error["lock"]["suggested_next_command"]
            .as_str()
            .unwrap()
            .contains("install doctor")
    );
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains("supersecrettoken")
    );
}

#[test]
fn installed_doctor_reports_active_and_stale_lock_without_mutation() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    write_lock(target_root.path(), 1);

    let output = cmd(cfg.path())
        .args(["--json", "install", "doctor", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["target_exists"].as_bool(), Some(true));
    assert_eq!(json["lock"]["exists"].as_bool(), Some(true));
    assert_eq!(json["lock"]["stale"].as_bool(), Some(true));
    assert_eq!(json["lock"]["metadata"]["pid"].as_u64(), Some(4242));
    assert!(
        target_root
            .child(".agentstack-install.lock")
            .path()
            .is_dir()
    );

    cmd(cfg.path())
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("install target local"))
        .stdout(predicate::str::contains("lock stale: yes"))
        .stdout(predicate::str::contains(
            "next: agentstack install unlock --target local",
        ))
        .stdout(predicate::str::contains("Next command:").not());
}

#[test]
fn installed_unlock_removes_stale_lock() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    write_lock(target_root.path(), 1);

    cmd(cfg.path())
        .args(["install", "unlock", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed stale install lock"));

    target_root
        .child(".agentstack-install.lock")
        .assert(predicate::path::missing());
}

#[test]
fn installed_unlock_refuses_fresh_lock_without_force() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    write_lock(target_root.path(), 0);

    cmd(cfg.path())
        .args(["install", "unlock", "--target", "local"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to remove fresh-looking install lock",
        ));

    target_root
        .child(".agentstack-install.lock")
        .assert(predicate::path::is_dir());
}

#[test]
fn installed_unlock_force_removes_fresh_lock_but_not_installed_skills() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    let skill = make_skill(&tmp, "kept-skill");
    cmd(cfg.path())
        .args(["skill", "install"])
        .arg(skill.path())
        .args(["--target", "local"])
        .assert()
        .success();
    write_lock(target_root.path(), 0);

    cmd(cfg.path())
        .args(["install", "unlock", "--target", "local", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "warning: --force bypassed the stale-lock age check",
        ));

    target_root
        .child(".agentstack-install.lock")
        .assert(predicate::path::missing());
    target_root
        .child("kept-skill")
        .child("SKILL.md")
        .assert(predicate::path::is_file());
}

#[test]
fn installed_doctor_does_not_emit_tokens() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target_root = tmp.child("target");
    set_local_target(cfg.path(), target_root.path());
    write_lock(target_root.path(), 1);

    cmd(cfg.path())
        .env("AGENTSTACK_TOKEN", "supersecrettoken")
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("supersecrettoken").not())
        .stderr(predicate::str::contains("supersecrettoken").not());
}
