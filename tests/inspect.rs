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

fn write_full_skill(dir: &ChildPath) {
    dir.create_dir_all().unwrap();
    dir.child("SKILL.md").write_str(FULL_SKILL_MD).unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        dir.child(sub).create_dir_all().unwrap();
    }
}

#[test]
fn inspect_text_prints_metadata() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("my-skill");
    write_full_skill(&target);
    target
        .child("references/api.md")
        .write_str("# API — see references/api.md\n")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("name:        my-skill"))
        .stdout(predicate::str::contains(
            "description: Use when foo happens",
        ))
        .stdout(predicate::str::contains("Purpose"))
        .stdout(predicate::str::contains("references"))
        .stdout(predicate::str::contains("api.md"));
}

#[test]
fn inspect_lists_unknown_files() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("with-unknown");
    write_full_skill(&target);
    target.child("NOTES.md").write_str("notes").unwrap();
    target.child("custom").create_dir_all().unwrap();
    target.child("custom/extra.txt").write_str("x").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("unknown files"))
        .stdout(predicate::str::contains("NOTES.md"))
        .stdout(predicate::str::contains("extra.txt"));
}

#[test]
fn inspect_does_not_fail_when_skill_md_missing() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("empty");
    target.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect"])
        .arg(target.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("missing_skill_md"));
}

#[test]
fn inspect_json_emits_parseable_json() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("my-skill");
    write_full_skill(&target);
    target
        .child("references/api.md")
        .write_str("# API\n")
        .unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect", "--json"])
        .arg(target.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("inspect --json should be valid JSON");

    // Top-level shape — every documented field must be present, even when null.
    for key in [
        "name",
        "description",
        "path",
        "skill_md",
        "directories",
        "unknown_files",
        "errors",
        "warnings",
        "package_hash",
    ] {
        assert!(
            json.get(key).is_some(),
            "missing top-level key `{key}` in JSON output: {json}"
        );
    }

    assert_eq!(json["name"], "my-skill");
    assert_eq!(json["description"], "Use when foo happens");

    let package_hash = json["package_hash"]
        .as_str()
        .expect("package_hash should be a string when packaging succeeds");
    assert_eq!(
        package_hash.len(),
        64,
        "package_hash should be a 64-char SHA-256 hex string, got {package_hash:?}"
    );
    assert!(
        package_hash
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "package_hash should be lowercase hex, got {package_hash:?}"
    );

    let archive = tmp.child("my-skill.tar.gz");
    let pack_output = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "pack", "--json", "--no-cache", "--out"])
        .arg(archive.path())
        .arg(target.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let pack_json: Value = serde_json::from_slice(&pack_output).unwrap();
    assert_eq!(
        pack_json["sha256"].as_str().unwrap(),
        package_hash,
        "inspect package_hash should match `pack --json` sha256 for the same fixture"
    );

    let skill_md = &json["skill_md"];
    assert!(skill_md.is_object(), "skill_md should be an object");
    assert!(skill_md["char_count"].as_u64().unwrap() > 0);
    let sections: Vec<&str> = skill_md["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        sections,
        vec![
            "Purpose",
            "When to Use",
            "Instructions",
            "Output",
            "Boundaries"
        ]
    );

    // Every standard subdir reported.
    let directories = &json["directories"];
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        let entry = &directories[sub];
        assert!(entry.is_object(), "directories.{sub} should be an object");
        assert!(entry["present"].as_bool().is_some());
        assert!(entry["files"].is_array());
    }
    assert!(directories["references"]["present"].as_bool().unwrap());
    let ref_files: Vec<&str> = directories["references"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(ref_files.contains(&"api.md"));

    assert!(json["errors"].is_array());
    assert!(json["warnings"].is_array());
    assert!(json["unknown_files"].is_array());
}

#[test]
fn inspect_json_includes_typed_warnings() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("warns");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str("---\nname: ok\ndescription: Use when foo\n---\n")
        .unwrap();
    // No subdirs, no sections — many lint warnings.

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect", "--json"])
        .arg(target.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let warnings = json["warnings"].as_array().expect("warnings array");
    let codes: Vec<&str> = warnings
        .iter()
        .map(|w| w["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"missing_section_purpose"));
    assert!(codes.contains(&"no_examples_directory"));
    assert!(codes.contains(&"no_references_directory"));
    for w in warnings {
        assert!(w["message"].is_string());
    }
}

#[test]
fn inspect_json_reports_validation_errors() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("invalid");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str("---\nname: Bad_Name\ndescription: Use when foo\n---\n")
        .unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect", "--json"])
        .arg(target.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let errors = json["errors"].as_array().expect("errors array");
    let codes: Vec<&str> = errors.iter().map(|e| e["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"invalid_name"));
    assert_eq!(
        json["package_hash"],
        Value::Null,
        "package_hash must be null when the skill fails validation"
    );
}

#[cfg(unix)]
#[test]
fn inspect_missing_directory_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.child("does-not-exist");

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect"])
        .arg(missing.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("errors:"))
        .stdout(predicate::str::contains("not_a_directory"))
        .stderr(predicate::str::contains("is not a skill directory"));
}

#[test]
fn inspect_json_missing_directory_emits_only_error_envelope() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.child("does-not-exist");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["--json", "skill", "inspect"])
        .arg(missing.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty());

    let json: Value = serde_json::from_slice(&assert.get_output().stderr)
        .expect("stderr should be a single JSON error envelope");
    assert_eq!(json["error"]["code"], "not_a_directory");
    assert_eq!(json["error"]["action"], "inspect_skill");
}

#[test]
fn json_output_includes_position_field_when_present_only() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.child("actual-name");
    target.create_dir_all().unwrap();
    target
        .child("SKILL.md")
        .write_str("---\nname: other-name\ndescription: Use when foo\n---\n")
        .unwrap();
    std::os::unix::fs::symlink("SKILL.md", target.child("link.md").path()).unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "inspect", "--json"])
        .arg(target.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let errors = json["errors"].as_array().expect("errors array");
    let name_mismatch = errors
        .iter()
        .find(|e| e["code"].as_str() == Some("name_mismatch"))
        .expect("name_mismatch error");
    assert_eq!(name_mismatch["position"]["line"], 2);
    assert_eq!(name_mismatch["position"]["col"], 1);

    let unsupported = errors
        .iter()
        .find(|e| e["code"].as_str() == Some("unsupported_top_level_entry"))
        .expect("unsupported_top_level_entry error");
    assert!(unsupported.get("position").is_none());
}
