use std::path::Path;

use agentstack::package::PackageHash;
use agentstack::receipt::{
    InstallReceipt, InstallVia, RECEIPT_SCHEMA_VERSION, ReceiptSourceType, StackInstallReceipt,
    StackInstallReceiptItem, receipt_path, write_receipt_to_dir, write_stack_receipt,
};
use agentstack::registry::Visibility;
use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

fn make_skill(parent: &TempDir, name: &str, description: &str) -> ChildPath {
    let body = format!(
        "---\nname: {name}\ndescription: Use when {description}\n---\n\n# Purpose\n\nThe {name} skill.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
    );
    let target = parent.child(name);
    target.create_dir_all().unwrap();
    target.child("SKILL.md").write_str(&body).unwrap();
    target
}

fn cmd(cfg_dir: &Path, home_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("agentstack").unwrap();
    command
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir)
        .env("HOME", home_dir)
        .env_remove("AGENTSTACK_TOKEN");
    command
}

fn set_target(cfg_dir: &Path, home_dir: &Path, target: &str, path: &Path) {
    cmd(cfg_dir, home_dir)
        .args(["target", "set", target, "--path", path.to_str().unwrap()])
        .assert()
        .success();
}

fn install_skill(cfg_dir: &Path, home_dir: &Path, skill: &ChildPath, target: &str) {
    cmd(cfg_dir, home_dir)
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            target,
        ])
        .assert()
        .success();
}

fn hash(hex: &str) -> PackageHash {
    PackageHash {
        algorithm: "sha256".to_string(),
        hex: hex.to_string(),
    }
}

#[test]
fn uninstall_removes_skill_with_receipt() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "alpha", "alpha is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "uninstall", "alpha", "--target", "local", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "removed skill `alpha` from target `local`",
        ));

    dest_root.child("alpha").assert(predicate::path::missing());
}

#[test]
fn stack_uninstall_explains_removed_child_cleanup() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let child_dir = dest_root.child("stack-child");
    child_dir.create_dir_all().unwrap();
    child_dir
        .child("SKILL.md")
        .write_str("# Stack Child\n")
        .unwrap();

    let via = InstallVia {
        kind: "stack".to_string(),
        org: "acme".to_string(),
        stack: "engineering-default".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
    };
    let child_receipt = InstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        skill_name: "stack-child".to_string(),
        source_type: ReceiptSourceType::Registry,
        source_ref: "acme/stack-child".to_string(),
        registry_url: Some("https://registry.agentstack.gg".to_string()),
        org: Some("acme".to_string()),
        version: Some("1".to_string()),
        hash: Some("sha256:abc".to_string()),
        content_hash: Some("sha256:abc".to_string()),
        target: "local".to_string(),
        installed_path: child_dir.path().to_path_buf(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        installed_by: Some("octocat".to_string()),
        installed_via: Some(via.clone()),
        installed_via_stacks: vec![via],
    };
    write_receipt_to_dir(child_dir.path(), &child_receipt).unwrap();

    let stack_receipt = StackInstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        kind: "stack".to_string(),
        org: "acme".to_string(),
        stack: "engineering-default".to_string(),
        registry_url: Some("https://registry.agentstack.gg".to_string()),
        visibility: Visibility::Org,
        team: None,
        resolved_at: "2026-01-01T00:00:00Z".to_string(),
        manifest_hash: hash("manifest"),
        target: "local".to_string(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        installed_by: Some("octocat".to_string()),
        items: vec![StackInstallReceiptItem {
            skill: "stack-child".to_string(),
            version_id: "ver_1".to_string(),
            version: "1".to_string(),
            archive_hash: hash("abc"),
            install_path: child_dir.path().to_path_buf(),
            installed_receipt_path: receipt_path(child_dir.path()),
        }],
    };
    write_stack_receipt(dest_root.path(), &stack_receipt).unwrap();

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "stack",
            "uninstall",
            "acme/engineering-default",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "uninstalled stack `acme/engineering-default` from target `local`",
        ))
        .stdout(predicate::str::contains("removed child skills: 1"))
        .stdout(predicate::str::contains(
            "removed child skills are already gone; no separate skill uninstall is needed.",
        ));

    child_dir.assert(predicate::path::missing());
    dest_root
        .child(".agentstack-stacks")
        .assert(predicate::path::missing());
}

fn write_stack_receipt_for_child(dest_root: &ChildPath, child_dir: &ChildPath) {
    let stack_receipt = StackInstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        kind: "stack".to_string(),
        org: "acme".to_string(),
        stack: "engineering-default".to_string(),
        registry_url: Some("https://registry.agentstack.gg".to_string()),
        visibility: Visibility::Org,
        team: None,
        resolved_at: "2026-01-01T00:00:00Z".to_string(),
        manifest_hash: hash("manifest"),
        target: "local".to_string(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        installed_by: Some("octocat".to_string()),
        items: vec![StackInstallReceiptItem {
            skill: "stack-child".to_string(),
            version_id: "ver_1".to_string(),
            version: "1".to_string(),
            archive_hash: hash("abc"),
            install_path: child_dir.path().to_path_buf(),
            installed_receipt_path: receipt_path(child_dir.path()),
        }],
    };
    write_stack_receipt(dest_root.path(), &stack_receipt).unwrap();
}

#[test]
fn stack_uninstall_force_leaves_receiptless_child_in_place() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let child_dir = dest_root.child("stack-child");
    child_dir.create_dir_all().unwrap();
    child_dir
        .child("SKILL.md")
        .write_str("# Stack Child\n")
        .unwrap();
    // No child install receipt: agentstack has no proof it owns this path.
    write_stack_receipt_for_child(&dest_root, &child_dir);

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "stack",
            "uninstall",
            "acme/engineering-default",
            "--target",
            "local",
            "--yes",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("left in place: stack-child"))
        .stdout(predicate::str::contains("no install receipt at"));

    child_dir.assert(predicate::path::is_dir());
    child_dir
        .child("SKILL.md")
        .assert(predicate::path::is_file());
    dest_root
        .child(".agentstack-stacks")
        .assert(predicate::path::missing());
}

#[test]
fn stack_uninstall_force_leaves_child_with_unreadable_receipt_in_place() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let child_dir = dest_root.child("stack-child");
    child_dir.create_dir_all().unwrap();
    child_dir
        .child("SKILL.md")
        .write_str("# Stack Child\n")
        .unwrap();
    child_dir
        .child(".agentstack-install.json")
        .write_str("{ not json")
        .unwrap();
    write_stack_receipt_for_child(&dest_root, &child_dir);

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "stack",
            "uninstall",
            "acme/engineering-default",
            "--target",
            "local",
            "--yes",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("left in place: stack-child"))
        .stdout(predicate::str::contains("is unreadable"));

    child_dir.assert(predicate::path::is_dir());
    child_dir
        .child("SKILL.md")
        .assert(predicate::path::is_file());
    dest_root
        .child(".agentstack-stacks")
        .assert(predicate::path::missing());
}

#[test]
fn stack_uninstall_accepts_org_qualified_ref() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let child_dir = dest_root.child("stack-child");
    child_dir.create_dir_all().unwrap();
    child_dir
        .child("SKILL.md")
        .write_str("# Stack Child\n")
        .unwrap();

    let via = InstallVia {
        kind: "stack".to_string(),
        org: "acme".to_string(),
        stack: "engineering-default".to_string(),
        manifest_hash: "sha256:manifest".to_string(),
    };
    let child_receipt = InstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        skill_name: "stack-child".to_string(),
        source_type: ReceiptSourceType::Registry,
        source_ref: "acme/stack-child".to_string(),
        registry_url: Some("https://registry.agentstack.gg".to_string()),
        org: Some("acme".to_string()),
        version: Some("1".to_string()),
        hash: Some("sha256:abc".to_string()),
        content_hash: Some("sha256:abc".to_string()),
        target: "local".to_string(),
        installed_path: child_dir.path().to_path_buf(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        installed_by: Some("octocat".to_string()),
        installed_via: Some(via.clone()),
        installed_via_stacks: vec![via],
    };
    write_receipt_to_dir(child_dir.path(), &child_receipt).unwrap();

    let stack_receipt = StackInstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        kind: "stack".to_string(),
        org: "acme".to_string(),
        stack: "engineering-default".to_string(),
        registry_url: Some("https://registry.agentstack.gg".to_string()),
        visibility: Visibility::Org,
        team: None,
        resolved_at: "2026-01-01T00:00:00Z".to_string(),
        manifest_hash: hash("manifest"),
        target: "local".to_string(),
        installed_at: "2026-01-01T00:00:00Z".to_string(),
        installed_by: Some("octocat".to_string()),
        items: vec![StackInstallReceiptItem {
            skill: "stack-child".to_string(),
            version_id: "ver_1".to_string(),
            version: "1".to_string(),
            archive_hash: hash("abc"),
            install_path: child_dir.path().to_path_buf(),
            installed_receipt_path: receipt_path(child_dir.path()),
        }],
    };
    write_stack_receipt(dest_root.path(), &stack_receipt).unwrap();

    let output = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "stack",
            "uninstall",
            "acme/engineering-default",
            "--target",
            "local",
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["kind"].as_str(), Some("stack"));
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["stack"].as_str(), Some("engineering-default"));
    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["dry_run"], Value::Bool(true));
    child_dir.assert(predicate::path::is_dir());
}

#[test]
fn skill_uninstall_requires_target() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "single", "single is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "uninstall", "single", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));

    dest_root.child("single").assert(predicate::path::is_dir());
}

#[test]
fn skill_uninstall_without_target_fails_before_target_scan() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let local_root = tmp.child("local");
    let codex_root = tmp.child("codex");
    set_target(cfg_dir.path(), home_dir.path(), "local", local_root.path());
    set_target(cfg_dir.path(), home_dir.path(), "codex", codex_root.path());

    let skill = make_skill(&tmp, "multi", "multi is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "codex");

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "uninstall", "multi", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));

    local_root.child("multi").assert(predicate::path::is_dir());
    codex_root.child("multi").assert(predicate::path::is_dir());
}

#[test]
fn uninstall_noninteractive_without_yes_refuses_and_keeps_skill() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "needs-yes", "needs yes is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--no-input",
            "skill",
            "uninstall",
            "needs-yes",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "uninstall requires --yes when stdin/stderr is not a TTY",
        ));

    dest_root
        .child("needs-yes")
        .assert(predicate::path::is_dir());
}

#[test]
fn uninstall_fails_when_not_installed() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "missing",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "skill `missing` is not installed at",
        ))
        .stderr(predicate::str::contains(
            "next: agentstack install list --target local",
        ));
}

#[test]
fn uninstall_missing_skill_json_includes_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let assert = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "missing",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("uninstall"));
    assert_eq!(json["error"]["resource"].as_str(), Some("missing"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack install list --target local")
    );
    assert!(
        !json["error"]["next_command"]
            .as_str()
            .unwrap()
            .contains('<')
    );
}

#[test]
fn stack_uninstall_missing_json_includes_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let assert = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "stack",
            "uninstall",
            "acme/missing-stack",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("stack_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("uninstall"));
    assert_eq!(
        json["error"]["resource"].as_str(),
        Some("acme/missing-stack")
    );
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack install list --kind stack --target local")
    );
    assert!(
        !json["error"]["next_command"]
            .as_str()
            .unwrap()
            .contains('<')
    );
}

#[test]
fn uninstall_refuses_without_receipt() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());
    let orphan = dest_root.child("orphan");
    orphan.create_dir_all().unwrap();
    orphan.child("SKILL.md").write_str("no receipt").unwrap();

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "uninstall", "orphan", "--target", "local", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no install receipt was found"))
        .stderr(predicate::str::contains("--force"))
        .stderr(predicate::str::contains(
            "next: agentstack skill uninstall orphan --target local --force --yes",
        ));
    orphan.assert(predicate::path::is_dir());

    let assert = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "orphan",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("uninstall"));
    assert_eq!(json["error"]["resource"].as_str(), Some("orphan"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack skill uninstall orphan --target local --force --yes")
    );
    assert!(
        !json["error"]["next_command"]
            .as_str()
            .unwrap()
            .contains('<')
    );
    orphan.assert(predicate::path::is_dir());

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "orphan",
            "--target",
            "local",
            "--force",
            "--yes",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "warning: no install receipt found",
        ))
        .stdout(predicate::str::contains(
            "removed skill `orphan` from target `local`",
        ));
    orphan.assert(predicate::path::missing());
}

#[test]
fn uninstall_json_output_shape() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "json-skill", "json skill is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    let output = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "json-skill",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["removed"]["skill"].as_str(), Some("json-skill"));
    assert_eq!(json["removed"]["target"].as_str(), Some("local"));
    assert!(
        json["removed"]["path"]
            .as_str()
            .unwrap()
            .ends_with("dest/json-skill")
    );
    assert_eq!(json["source_type"].as_str(), Some("local"));
    assert!(json["source_ref"].as_str().is_some());
    assert_eq!(json["version"], Value::Null);
    assert!(json["hash"].as_str().unwrap().starts_with("sha256:"));
    dest_root
        .child("json-skill")
        .assert(predicate::path::missing());
}

#[test]
fn uninstall_dry_run_leaves_skill_in_place() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "dryrun", "dryrun is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "dryrun",
            "--target",
            "local",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "would remove skill `dryrun` from target `local`",
        ))
        .stdout(predicate::str::contains("dry run; nothing removed."));

    dest_root.child("dryrun").assert(predicate::path::is_dir());
}

#[test]
fn uninstall_dry_run_json_shape() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "dryrun-json", "dryrun-json is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    let output = cmd(cfg_dir.path(), home_dir.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "dryrun-json",
            "--target",
            "local",
            "--dry-run",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();

    assert_eq!(json["would_remove"]["skill"].as_str(), Some("dryrun-json"));
    assert_eq!(json["would_remove"]["target"].as_str(), Some("local"));
    assert_eq!(json["dry_run"], Value::Bool(true));
    dest_root
        .child("dryrun-json")
        .assert(predicate::path::is_dir());
}

#[test]
fn uninstall_refuses_on_receipt_path_mismatch() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "stale", "stale is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    let receipt_file = dest_root.child("stale").child(".agentstack-install.json");
    let raw = std::fs::read_to_string(receipt_file.path()).unwrap();
    let mut receipt: Value = serde_json::from_str(&raw).unwrap();
    receipt["installed_path"] = Value::String("/tmp/somewhere-else/stale".to_string());
    std::fs::write(
        receipt_file.path(),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "uninstall", "stale", "--target", "local", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not match resolved path"))
        .stderr(predicate::str::contains("--force"));
    dest_root.child("stale").assert(predicate::path::is_dir());

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "stale",
            "--target",
            "local",
            "--force",
            "--yes",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "removing the resolved path because --force was set",
        ));
    dest_root.child("stale").assert(predicate::path::missing());
}

#[test]
fn uninstall_skips_prompt_with_yes() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "prompt-skip", "prompt skip is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "prompt-skip",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "removed skill `prompt-skip` from target `local`",
        ));

    dest_root
        .child("prompt-skip")
        .assert(predicate::path::missing());
}

#[test]
fn skill_uninstall_refuses_stack_owned_child() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home_dir = tmp.child("home");
    home_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    set_target(cfg_dir.path(), home_dir.path(), "local", dest_root.path());

    let skill = make_skill(&tmp, "stack-child", "stack child is needed");
    install_skill(cfg_dir.path(), home_dir.path(), &skill, "local");

    let receipt_file = dest_root
        .child("stack-child")
        .child(".agentstack-install.json");
    let raw = std::fs::read_to_string(receipt_file.path()).unwrap();
    let mut receipt: Value = serde_json::from_str(&raw).unwrap();
    let via = serde_json::json!({
        "kind": "stack",
        "org": "acme",
        "stack": "engineering-default",
        "manifest_hash": "sha256:abc"
    });
    receipt["installed_via"] = via.clone();
    receipt["installed_via_stacks"] = Value::Array(vec![via]);
    std::fs::write(
        receipt_file.path(),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();

    cmd(cfg_dir.path(), home_dir.path())
        .args(["skill", "show", "stack-child", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("required by:"))
        .stdout(predicate::str::contains("stack acme/engineering-default"))
        .stdout(predicate::str::contains(
            "agentstack stack update acme/engineering-default --target local --check",
        ))
        .stdout(predicate::str::contains("agentstack skill update").not());

    cmd(cfg_dir.path(), home_dir.path())
        .args([
            "skill",
            "uninstall",
            "stack-child",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot remove stack-owned child skill `stack-child` directly",
        ))
        .stderr(predicate::str::contains(
            "agentstack stack uninstall acme/engineering-default --target local",
        ));

    dest_root
        .child("stack-child")
        .assert(predicate::path::is_dir());
}
