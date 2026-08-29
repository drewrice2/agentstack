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

fn write_stack_receipt(target_root: &ChildPath, target: &str, org: &str, stack: &str) {
    let receipt_dir = target_root
        .child(".agentstack-stacks")
        .child(org)
        .child(stack);
    receipt_dir.create_dir_all().unwrap();
    let receipt = serde_json::json!({
        "schema_version": 1,
        "kind": "stack_install",
        "org": org,
        "stack": stack,
        "visibility": "org",
        "resolved_at": "2026-05-09T00:00:00Z",
        "manifest_hash": {
            "algorithm": "sha256",
            "hex": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        },
        "target": target,
        "installed_at": "2026-05-09T00:00:00Z",
        "items": []
    });
    receipt_dir
        .child(".agentstack.json")
        .write_str(&serde_json::to_string_pretty(&receipt).unwrap())
        .unwrap();
}

fn configure_empty_local_target(cfg_dir: &ChildPath, target: &ChildPath) {
    target.create_dir_all().unwrap();
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["target", "set", "local", "--path"])
        .arg(target.path())
        .assert()
        .success();
}

#[test]
fn install_with_target_override_writes_under_destination() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("set target `local`"));

    let skill = make_skill(&tmp, "alpha", "alpha is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed skill alpha"))
        .stdout(predicate::str::contains("target: local"))
        .stdout(predicate::str::contains("destination:"))
        .stdout(predicate::str::contains("install tree hash: sha256:"))
        .stdout(predicate::str::contains("package hash:").not())
        .stdout(predicate::str::contains("receipt:"));

    dest_root
        .child("alpha")
        .child("SKILL.md")
        .assert(predicate::path::is_file());
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "list", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("HASH KIND"))
        .stdout(predicate::str::contains("install-tree"));
    let receipt_text = std::fs::read_to_string(
        dest_root
            .child("alpha")
            .child(".agentstack-install.json")
            .path(),
    )
    .unwrap();
    let receipt: Value = serde_json::from_str(&receipt_text).unwrap();
    assert_eq!(receipt["schema_version"].as_u64(), Some(1));
    assert_eq!(receipt["skill_name"].as_str(), Some("alpha"));
    assert_eq!(receipt["source_type"].as_str(), Some("local"));
    assert_eq!(receipt["target"].as_str(), Some("local"));
    assert!(receipt["hash"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(
        receipt["content_hash"], receipt["hash"],
        "local installs record the installed-tree hash as content_hash for drift detection"
    );
    assert!(
        receipt["installed_path"]
            .as_str()
            .unwrap()
            .ends_with("dest/alpha")
    );
}

#[test]
fn install_applies_platform_overlay_for_claude_code_target() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "claude-code",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "alpha", "alpha is needed");
    let overlay = skill.child("platform").child("claude-code");
    overlay.create_dir_all().unwrap();
    overlay
        .child("SKILL.md")
        .write_str(
            "---\nname: alpha\ndescription: Use when alpha is needed\n---\n\n# Purpose\n\nClaude Code overlay.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "claude-code",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["overlay"]["platform"].as_str(), Some("claude-code"));
    assert_eq!(json["overlay"]["files"].as_u64(), Some(1));
    assert_eq!(json["platform_warning"], Value::Null);

    let installed_manifest =
        std::fs::read_to_string(dest_root.child("alpha").child("SKILL.md").path()).unwrap();
    assert!(installed_manifest.contains("Claude Code overlay."));
    dest_root
        .child("alpha")
        .child("platform")
        .child("claude-code")
        .child("SKILL.md")
        .assert(predicate::path::is_file());

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "claude-code",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "applied platform overlay: claude-code (1 file)",
        ));
}

#[test]
fn install_why_explains_direct_local_install() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "common-review", "common review is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "why", "common-review", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("skill: common-review"))
        .stdout(predicate::str::contains(
            "installed by:\n  - direct install",
        ))
        .stdout(predicate::str::contains("direct install: yes"))
        .stdout(predicate::str::contains("safe to remove: yes"))
        .stdout(predicate::str::contains(
            "next: agentstack skill uninstall common-review --target local --dry-run",
        ));

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "install",
            "why",
            "common-review",
            "--target",
            "local",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["safe_to_remove"].as_bool(), Some(true));
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill uninstall common-review --target local --dry-run")
    );
}

#[test]
fn install_why_explains_shared_stack_referrers_json() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "common-review", "common review is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let receipt_file = dest_root
        .child("common-review")
        .child(".agentstack-install.json");
    let mut receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(receipt_file.path()).unwrap()).unwrap();
    let stack_a = serde_json::json!({
        "kind": "stack",
        "org": "acme",
        "stack": "engineering-default",
        "manifest_hash": "sha256:aaa"
    });
    let stack_b = serde_json::json!({
        "kind": "stack",
        "org": "acme",
        "stack": "frontend-default",
        "manifest_hash": "sha256:bbb"
    });
    receipt["installed_via"] = stack_a.clone();
    receipt["installed_via_stacks"] = Value::Array(vec![stack_a, stack_b]);
    std::fs::write(
        receipt_file.path(),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "install",
            "why",
            "common-review",
            "--target",
            "local",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["skill"].as_str(), Some("common-review"));
    assert_eq!(json["source_type"].as_str(), Some("local"));
    assert_eq!(json["current_version_known"].as_bool(), Some(false));
    assert_eq!(json["current_version"], Value::Null);
    assert_eq!(
        json["registry_check_status"].as_str(),
        Some("local_install")
    );
    assert_eq!(json["provenance"].as_str(), Some("stack"));
    assert_eq!(json["direct_remove_safe"].as_bool(), Some(false));
    assert_eq!(json["installed_by"]["direct"].as_bool(), Some(false));
    assert_eq!(json["safe_to_remove"].as_bool(), Some(false));
    assert_eq!(
        json["required_by_stacks"].as_array().unwrap(),
        &vec![
            Value::String("acme/engineering-default".to_string()),
            Value::String("acme/frontend-default".to_string()),
        ]
    );
    assert_eq!(
        json["installed_by"]["stacks"].as_array().unwrap(),
        &vec![
            Value::String("acme/engineering-default".to_string()),
            Value::String("acme/frontend-default".to_string()),
        ]
    );
    assert_eq!(json["reason"].as_str(), Some("still required by 2 stacks"));
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill show common-review --target local")
    );
    assert!(
        !json["next_command"].as_str().unwrap().contains('<'),
        "next_command must be concrete for JSON"
    );
}

#[test]
fn install_why_explains_stack_owned_human_output() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "stack-child", "stack child is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let receipt_file = dest_root
        .child("stack-child")
        .child(".agentstack-install.json");
    let mut receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(receipt_file.path()).unwrap()).unwrap();
    let via = serde_json::json!({
        "kind": "stack",
        "org": "acme",
        "stack": "engineering-default",
        "manifest_hash": "sha256:aaa"
    });
    receipt["installed_via"] = via.clone();
    receipt["installed_via_stacks"] = Value::Array(vec![via]);
    std::fs::write(
        receipt_file.path(),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "why", "stack-child", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed by:\n  - stack acme/engineering-default",
        ))
        .stdout(predicate::str::contains("direct install: no"))
        .stdout(predicate::str::contains(
            "safe to remove: no, still required by 1 stack",
        ));
}

#[test]
fn install_why_json_errors_distinguish_missing_and_invalid_receipts() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let missing = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "install",
            "why",
            "missing-skill",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let missing_json: Value = serde_json::from_slice(&missing).unwrap();
    assert_eq!(
        missing_json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(
        missing_json["error"]["action"].as_str(),
        Some("install_why")
    );
    assert_eq!(
        missing_json["error"]["next_command"].as_str(),
        Some("agentstack install list --target local")
    );

    let skill = make_skill(&tmp, "bad-receipt", "bad receipt is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();
    dest_root
        .child("bad-receipt")
        .child(".agentstack-install.json")
        .write_str("{")
        .unwrap();

    let invalid = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "install",
            "why",
            "bad-receipt",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let invalid_json: Value = serde_json::from_slice(&invalid).unwrap();
    assert_eq!(
        invalid_json["error"]["code"].as_str(),
        Some("install_receipt_invalid")
    );
    assert_eq!(
        invalid_json["error"]["action"].as_str(),
        Some("install_why")
    );
}

#[test]
fn install_prefers_local_skill_dir_over_matching_remote_ref() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = tmp.child("acme").child("my-skill");
    skill.create_dir_all().unwrap();
    skill
        .child("SKILL.md")
        .write_str(
            "---\nname: my-skill\ndescription: Use when my skill is needed\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "install", "acme/my-skill", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("installed skill my-skill"));

    dest_root
        .child("my-skill")
        .child("SKILL.md")
        .assert(predicate::path::is_file());
    let receipt_text = std::fs::read_to_string(
        dest_root
            .child("my-skill")
            .child(".agentstack-install.json")
            .path(),
    )
    .unwrap();
    let receipt: Value = serde_json::from_str(&receipt_text).unwrap();
    assert_eq!(receipt["source_type"].as_str(), Some("local"));
}

#[test]
fn skill_install_without_target_requires_explicit_target() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();

    let skill = make_skill(&tmp, "auto-zero", "auto zero is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "install", skill.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));

    let dest_root = tmp.child("dest");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "auto-one", "auto one is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "install", skill.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));

    let codex_root = tmp.child("codex");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "codex",
            "--path",
            codex_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "auto-many", "auto many is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--no-input",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));
}

#[test]
fn install_refuses_overwrite_of_foreign_directory_without_force() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    let claude_root = tmp.child("claude");
    let codex_root = tmp.child("codex");

    for (target, path) in [
        ("local", dest_root.path()),
        ("claude-code", claude_root.path()),
        ("codex", codex_root.path()),
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
            .args(["target", "set", target, "--path", path.to_str().unwrap()])
            .assert()
            .success();
    }

    let skill = make_skill(&tmp, "beta", "beta is needed");

    // Pre-create a foreign directory at the install destination — no
    // AgentStack receipt, so install must refuse without --force.
    let foreign = dest_root.child("beta");
    foreign.create_dir_all().unwrap();
    foreign.child("legacy.txt").write_str("foreign").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing to overwrite"));
}

#[test]
fn install_local_reinstall_without_force_refuses_and_preserves_files() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    let claude_root = tmp.child("claude");
    let codex_root = tmp.child("codex");

    for (target, path) in [
        ("local", dest_root.path()),
        ("claude-code", claude_root.path()),
        ("codex", codex_root.path()),
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
            .args(["target", "set", target, "--path", path.to_str().unwrap()])
            .assert()
            .success();
    }

    let skill = make_skill(&tmp, "beta", "beta is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let user_file = dest_root.child("beta").child("user-notes.txt");
    user_file.write_str("keep me").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "refusing to replace existing install",
        ))
        .stderr(predicate::str::contains("--force"));

    user_file.assert(predicate::path::is_file());
}

#[test]
fn install_force_overwrites_existing() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "gamma", "gamma is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let stale = dest_root.child("gamma").child("stale.txt");
    stale.write_str("old").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
            "--force",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("overwrote: yes (--force)"))
        .stdout(predicate::str::contains("warnings:"));

    stale.assert(predicate::path::missing());
}

#[test]
fn installed_list_and_inspect_read_local_receipt_json() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "listed", "listed is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let list = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "install", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&list).unwrap();
    let installed = json["installed"].as_array().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0]["skill_name"].as_str(), Some("listed"));
    assert_eq!(installed[0]["source_type"].as_str(), Some("local"));

    // The human-readable list leads with a labeled header row (matching the
    // header convention of `skill list` / `target list`).
    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "list"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("SKILL")
                .and(predicate::str::contains("TARGET"))
                .and(predicate::str::contains("SOURCE"))
                .and(predicate::str::contains("VERSION"))
                .and(predicate::str::contains("INSTALLED")),
        );

    let inspect = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "skill", "show", "listed", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&inspect).unwrap();
    assert_eq!(json["receipt"]["skill_name"].as_str(), Some("listed"));
    assert_eq!(json["receipt"]["source_type"].as_str(), Some("local"));
    assert_eq!(json["validation"]["ok"].as_bool(), Some(true));

    let resource_inspect = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "skill", "show", "listed", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&resource_inspect).unwrap();
    assert_eq!(json["receipt"]["skill_name"].as_str(), Some("listed"));
    assert_eq!(json["receipt"]["source_type"].as_str(), Some("local"));
    assert_eq!(json["validation"]["ok"].as_bool(), Some(true));
}

#[test]
fn install_show_stack_with_target_does_not_scan_other_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let local_root = tmp.child("local");
    let codex_root = tmp.child("codex");
    let bad_other_target = tmp.child("not-a-directory");
    bad_other_target.write_str("not a directory").unwrap();

    for (target, path) in [
        ("claude-code", bad_other_target.path()),
        ("codex", codex_root.path()),
        ("local", local_root.path()),
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
            .args(["target", "set", target, "--path", path.to_str().unwrap()])
            .assert()
            .success();
    }

    write_stack_receipt(&local_root, "local", "acme", "engineering-default");
    write_stack_receipt(&codex_root, "codex", "other", "engineering-default");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["stack", "show", "engineering-default", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed stack acme/engineering-default",
        ))
        .stdout(predicate::str::contains("target: local"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "stack",
            "show",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed stack acme/engineering-default",
        ))
        .stdout(predicate::str::contains("target: local"));

    let resource_inspect = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json",
            "stack",
            "show",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&resource_inspect).unwrap();
    assert_eq!(json["receipt"]["org"].as_str(), Some("acme"));
    assert_eq!(
        json["receipt"]["stack"].as_str(),
        Some("engineering-default")
    );
    assert!(json["receipt_path"].as_str().unwrap().contains("acme"));
}

#[test]
fn install_receipts_list_empty_prints_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    configure_empty_local_target(&cfg_dir, &tmp.child("empty-target"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["install", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no install receipts found."))
        .stdout(predicate::str::contains(
            "next: agentstack skill install <path> --target local",
        ));
}

#[test]
fn install_receipts_list_empty_target_filter_avoids_self_loop_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["install", "list", "--target", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "next: agentstack skill install <path> --target codex",
        ));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["install", "list", "--kind", "stack", "--target", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("next: agentstack stack list"));
}

#[test]
fn install_receipts_list_empty_json_includes_empty_state() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    configure_empty_local_target(&cfg_dir, &tmp.child("empty-target"));

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["--json", "install", "list"])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert!(json["installed"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no install receipts found.")
    );
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack skill install <path> --target local")
    );
}

#[test]
fn install_receipts_list_all_empty_json_includes_both_kinds() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    configure_empty_local_target(&cfg_dir, &tmp.child("empty-target"));

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["--json", "install", "list", "--kind", "all"])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert!(json["skills"].as_array().unwrap().is_empty());
    assert!(json["stacks"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no installed skills or stacks found.")
    );
}

#[test]
fn install_receipts_list_all_empty_human_uses_single_summary() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    configure_empty_local_target(&cfg_dir, &tmp.child("empty-target"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["install", "list", "--kind", "all"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no installed skills or stacks found.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill install <path> --target local",
        ))
        .stdout(predicate::str::contains("skills:").not())
        .stdout(predicate::str::contains("stacks:").not())
        .stdout(predicate::str::contains("Skills:").not())
        .stdout(predicate::str::contains("Stacks:").not());
}

#[test]
fn update_all_check_skips_local_installs_without_auth() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "local-update", "local update is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .args(["install", "update", "--all", "--target", "local", "--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains("local-update"))
        .stdout(predicate::str::contains("skipped"))
        .stdout(predicate::str::contains(
            "local installs are not registry-updateable",
        ))
        .stdout(predicate::str::contains("summary: updated 0"))
        .stderr(predicate::str::contains("not logged in").not());
}

#[test]
fn install_update_without_all_requires_batch_flag() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "update", "--target", "local", "--check"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--all"))
        .stderr(predicate::str::contains("pass a skill name").not());
}

#[test]
fn install_receipts_stack_list_empty_json_omits_self_referential_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "install", "list", "--kind", "stack"])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert!(json["installed"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no stack install receipts found.")
    );
    assert!(json["next_command"].is_null());
}

#[test]
fn install_receipts_stack_list_empty_json_target_omits_self_referential_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "--json", "install", "list", "--kind", "stack", "--target", "codex",
        ])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert!(json["installed"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no stack install receipts found.")
    );
    assert!(json["next_command"].is_null());
}

#[test]
fn installed_list_skips_corrupt_receipts() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");
    dest_root.create_dir_all().unwrap();
    let broken = dest_root.child("broken");
    broken.create_dir_all().unwrap();
    broken
        .child(".agentstack-install.json")
        .write_str("{not json")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .current_dir(tmp.path())
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no install receipts found."))
        .stderr(predicate::str::contains(
            "skipping unreadable install receipt",
        ));
}

#[test]
fn install_invalid_skill_fails_before_writing() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let bad = tmp.child("not-a-skill");
    bad.create_dir_all().unwrap();
    bad.child("notes.txt").write_str("hi").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            bad.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid skill"));

    // Nothing should have been written under the destination root.
    if dest_root.path().exists() {
        let mut iter = std::fs::read_dir(dest_root.path()).unwrap();
        assert!(iter.next().is_none(), "destination should be empty");
    }
}

#[test]
fn skill_install_rejects_removed_name_flag() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, "delta", "delta is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
            "--name",
            "delta-renamed",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--name"));

    dest_root
        .child("delta-renamed")
        .assert(predicate::path::missing());
    dest_root.child("delta").assert(predicate::path::missing());
}

#[test]
fn install_unknown_target_errors() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let skill = make_skill(&tmp, "epsilon", "epsilon is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "vscode",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown install target"));
}

#[test]
fn install_with_local_target_auto_registers_default_path() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let skill = make_skill(&tmp, "auto-reg", "auto-register is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("registered target `local`"));

    let expected_path = home.child(".agentstack").child("skills");
    expected_path
        .child("auto-reg")
        .child("SKILL.md")
        .assert(predicate::path::is_file());

    let config_text = std::fs::read_to_string(cfg_dir.child("config.toml").path()).unwrap();
    assert!(
        config_text.contains("local"),
        "config.toml should record the local override; got:\n{config_text}"
    );
    assert!(
        config_text.contains(expected_path.path().to_str().unwrap()),
        "config.toml should point at the registered path; got:\n{config_text}"
    );

    // Second run reuses the override and emits no `registered target` note.
    let skill2 = make_skill(&tmp, "auto-reg-2", "second is needed");
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args([
            "skill",
            "install",
            skill2.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("registered target").not());
}

#[test]
fn install_with_local_target_json_auto_registers_silently() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let skill = make_skill(&tmp, "auto-json", "auto-json is needed");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args([
            "--json",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("registered target").not());

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let payload: Value = serde_json::from_str(&stdout).expect("install --json stdout is JSON");
    assert_eq!(payload["target"].as_str(), Some("local"));
    assert_eq!(payload["target_source"].as_str(), Some("override"));

    let config_text = std::fs::read_to_string(cfg_dir.child("config.toml").path()).unwrap();
    assert!(
        config_text.contains("local"),
        "config.toml should record the local override; got:\n{config_text}"
    );
}

#[test]
fn install_with_unconfigured_user_target_requires_explicit_setup() {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let skill = make_skill(&tmp, "needs-setup", "setup is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "claude-code",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "target `claude-code` is not configured",
        ))
        .stderr(predicate::str::contains(
            "agentstack target setup claude-code --yes",
        ));

    home.child(".claude")
        .child("skills")
        .assert(predicate::path::missing());
    cfg_dir
        .child("config.toml")
        .assert(predicate::path::missing());
}

#[test]
fn install_without_target_still_errors_when_unconfigured() {
    // Regression guard: --target omitted with no configured targets must still
    // fail. We do NOT auto-pick a default target on the no-`--target` path.
    // For `agentstack skill install`, clap enforces this before runtime; the
    // canonical CLI flow therefore never reaches the runtime "no configured
    // usable install target" bail. This test pins the clap-level guard so the
    // no-auto-pick decision is not silently regressed by making `--target`
    // optional.
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let skill = make_skill(&tmp, "no-target", "no-target is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env("HOME", home.path())
        .args(["skill", "install", skill.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"));

    // The HOME/.claude path must NOT have been auto-created in this branch.
    let claude_root = home.child(".claude").child("skills");
    claude_root.assert(predicate::path::missing());
    let config_path = cfg_dir.child("config.toml");
    if config_path.path().exists() {
        let text = std::fs::read_to_string(config_path.path()).unwrap();
        assert!(
            !text.contains("claude-code"),
            "no-target install must not register any target; got:\n{text}"
        );
    }
}

/// Install a local skill into `local`, returning (tmp, cfg_dir, dest_root).
fn install_local_skill(name: &str, description: &str) -> (TempDir, ChildPath, ChildPath) {
    let tmp = TempDir::new().unwrap();
    let cfg_dir = tmp.child("cfg");
    cfg_dir.create_dir_all().unwrap();
    let dest_root = tmp.child("dest");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest_root.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let skill = make_skill(&tmp, name, description);
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();
    (tmp, cfg_dir, dest_root)
}

/// Rewrite the install receipt for `skill` under `dest_root` so it looks like a
/// registry install whose recorded hash is the *package* hash of the installed
/// files.
fn make_registry_receipt(dest_root: &ChildPath, skill: &str) {
    let installed = dest_root.child(skill);
    let pkg = agentstack::package::build_skill_package(installed.path()).unwrap();
    let recorded = agentstack::receipt::format_hash(&pkg.hash);
    let receipt_path = installed.child(".agentstack-install.json");
    let mut receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(receipt_path.path()).unwrap()).unwrap();
    let content_hash = receipt["hash"].clone();
    receipt["source_type"] = Value::from("registry");
    receipt["source_ref"] = Value::from(format!("acme/{skill}"));
    receipt["registry_url"] = Value::from("http://127.0.0.1:0");
    receipt["org"] = Value::from("acme");
    receipt["version"] = Value::from("1");
    receipt["hash"] = Value::from(recorded);
    receipt["content_hash"] = content_hash;
    std::fs::write(
        receipt_path.path(),
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();
}

#[test]
fn install_show_reports_no_drift_for_fresh_local_install() {
    // Local-source installs record a content hash too, so a fresh install
    // must report matching content rather than unknown.
    let (_tmp, cfg_dir, _dest_root) = install_local_skill("localdrift", "localdrift is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "localdrift", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("install tree hash: sha256:"))
        .stdout(predicate::str::contains("package hash:").not())
        .stdout(predicate::str::contains(
            "content: matches recorded package",
        ))
        .stdout(predicate::str::contains("content: modified").not());
    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "localdrift", "--target", "local", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["hash_kind"].as_str(), Some("install_tree"));
    assert_eq!(json["content_drifted"].as_bool(), Some(false));
}

#[test]
fn install_show_reports_drift_after_local_source_edit() {
    // Editing a local-source install must surface as drift, and the restore
    // hint must reinstall the source path (`skill update` rejects local
    // receipts).
    let (_tmp, cfg_dir, dest_root) = install_local_skill("localedit", "localedit is needed");

    let skill_md = dest_root.child("localedit").child("SKILL.md");
    let mut body = std::fs::read_to_string(skill_md.path()).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    std::fs::write(skill_md.path(), body).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "localedit", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: modified — installed files differ from recorded package",
        ))
        .stdout(predicate::str::contains("run `agentstack skill install "))
        .stdout(predicate::str::contains(
            "--target local --force` to restore",
        ))
        .stdout(predicate::str::contains("agentstack skill update").not());

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "localedit", "--target", "local", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["content_drifted"].as_bool(), Some(true));
}

#[test]
fn install_update_local_receipt_rejects_before_auth() {
    let (_tmp, cfg_dir, _dest_root) = install_local_skill("updateauth", "updateauth is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args([
            "skill",
            "update",
            "updateauth",
            "--target",
            "local",
            "--check",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot update `updateauth` from a local install receipt",
        ))
        .stderr(predicate::str::contains(
            "agentstack skill install <org>/updateauth --target local --force",
        ))
        .stderr(predicate::str::contains("not logged in").not());
}

#[test]
fn install_show_reports_no_drift_for_unedited_install() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("driftclean", "driftclean is needed");
    make_registry_receipt(&dest_root, "driftclean");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "driftclean", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("package hash: sha256:"))
        .stdout(predicate::str::contains(
            "content: matches recorded package",
        ));

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "driftclean", "--target", "local", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["content_drifted"].as_bool(), Some(false));
}

#[test]
fn install_show_treats_legacy_registry_receipt_without_content_hash_as_unknown() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("legacyhash", "legacyhash is needed");
    make_registry_receipt(&dest_root, "legacyhash");
    let receipt_path = dest_root
        .child("legacyhash")
        .child(".agentstack-install.json");
    let mut receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(receipt_path.path()).unwrap()).unwrap();
    receipt.as_object_mut().unwrap().remove("content_hash");
    std::fs::write(
        receipt_path.path(),
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "legacyhash", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: unknown (no recorded content hash)",
        ))
        .stdout(predicate::str::contains("content: modified").not());

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "legacyhash", "--target", "local", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert!(json["content_drifted"].is_null());
}

#[test]
fn install_doctor_verifies_content_for_local_install() {
    let (_tmp, cfg_dir, _dest_root) = install_local_skill("doclocal", "doclocal is needed");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: all installs match recorded packages",
        ))
        .stdout(predicate::str::contains("unverified").not());

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "install", "doctor", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        json["recorded_package_matches"][0]["skill"].as_str(),
        Some("doclocal")
    );
    assert!(json["drifted"].as_array().unwrap().is_empty());
    assert!(json["unknown"].as_array().unwrap().is_empty());
}

#[test]
fn install_doctor_reports_unknown_for_legacy_local_receipt() {
    // Legacy local receipts (written before content hashes were recorded for
    // local installs) must keep loading and stay unverified, not drifted.
    let (_tmp, cfg_dir, dest_root) = install_local_skill("doclegacy", "doclegacy is needed");
    let receipt_path = dest_root
        .child("doclegacy")
        .child(".agentstack-install.json");
    let mut receipt: Value =
        serde_json::from_str(&std::fs::read_to_string(receipt_path.path()).unwrap()).unwrap();
    receipt.as_object_mut().unwrap().remove("content_hash");
    std::fs::write(
        receipt_path.path(),
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: 0 matched recorded packages, 0 drifted, 1 unverified",
        ))
        .stdout(predicate::str::contains("doclegacy content unverified"));

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["--json", "install", "doctor", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert!(json["drifted"].as_array().unwrap().is_empty());
    assert_eq!(json["unknown"][0]["skill"].as_str(), Some("doclegacy"));
    assert_eq!(
        json["unknown"][0]["reason"].as_str(),
        Some("no recorded content hash")
    );
}

#[test]
fn install_doctor_warns_after_local_source_edit() {
    // A drifted local-source install must be flagged, with a restore hint
    // that reinstalls the source path rather than `skill update` (which
    // rejects local receipts).
    let (_tmp, cfg_dir, dest_root) = install_local_skill("docledit", "docledit is needed");

    let skill_md = dest_root.child("docledit").child("SKILL.md");
    let mut body = std::fs::read_to_string(skill_md.path()).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    std::fs::write(skill_md.path(), body).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[warn]"))
        .stdout(predicate::str::contains("docledit content modified"))
        .stdout(predicate::str::contains("run `agentstack skill install "))
        .stdout(predicate::str::contains("agentstack skill update").not());
}

#[test]
fn install_doctor_clean_when_no_drift() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("docclean", "docclean is needed");
    make_registry_receipt(&dest_root, "docclean");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: all installs match recorded packages",
        ))
        .stdout(predicate::str::contains("[warn]").not());
}

#[test]
fn install_doctor_skips_lifecycle_checks_without_token() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("doclife", "doclife is needed");
    make_registry_receipt(&dest_root, "doclife");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "note: registry lifecycle checks skipped",
        ))
        .stdout(predicate::str::contains("[fail]").not());

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["--json", "install", "doctor", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        json["lifecycle"][0]["code"].as_str(),
        Some("registry_lifecycle_skipped")
    );
    assert_eq!(json["lifecycle"][0]["status"].as_str(), Some("ok"));
    assert!(json["lifecycle"][0]["fix_command"].is_null());
}

#[test]
fn install_doctor_lifecycle_silent_for_local_installs() {
    let (_tmp, cfg_dir, _dest_root) = install_local_skill("doclocallife", "doclocallife is needed");

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["--json", "install", "doctor", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert!(json["lifecycle"].as_array().unwrap().is_empty());
}

#[test]
fn install_show_reports_drift_after_local_edit() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("driftedit", "driftedit is needed");
    make_registry_receipt(&dest_root, "driftedit");

    // Hand-edit a tracked (non-hidden) file under the install.
    let skill_md = dest_root.child("driftedit").child("SKILL.md");
    let mut body = std::fs::read_to_string(skill_md.path()).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    std::fs::write(skill_md.path(), body).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "driftedit", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "content: modified — installed files differ from recorded package",
        ));

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .args(["skill", "show", "driftedit", "--target", "local", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["content_drifted"].as_bool(), Some(true));
}

#[test]
fn install_doctor_warns_after_local_edit() {
    let (_tmp, cfg_dir, dest_root) = install_local_skill("docdrift", "docdrift is needed");
    make_registry_receipt(&dest_root, "docdrift");

    let skill_md = dest_root.child("docdrift").child("SKILL.md");
    let mut body = std::fs::read_to_string(skill_md.path()).unwrap();
    body.push_str("\n# Local hand edit\nAdded by a user.\n");
    std::fs::write(skill_md.path(), body).unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg_dir.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .args(["install", "doctor", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[warn]"))
        .stdout(predicate::str::contains("docdrift content modified"));
}
