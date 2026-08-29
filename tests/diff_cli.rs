use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;

fn write_skill(dir: &ChildPath, name: &str, purpose: &str) {
    dir.create_dir_all().unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: Use when working on {name} tasks\n---\n\n# Purpose\n\n{purpose}\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    dir.child("SKILL.md").write_str(&body).unwrap();
}

#[test]
fn skill_diff_local_json_does_not_need_registry_config() {
    let tmp = TempDir::new().unwrap();
    let left = tmp.child("left").child("alpha");
    let right = tmp.child("right").child("alpha");
    write_skill(&left, "alpha", "Old purpose.");
    write_skill(&right, "alpha", "New purpose.");

    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args([
            "--json",
            "skill",
            "diff",
            left.path().to_str().unwrap(),
            right.path().to_str().unwrap(),
        ])
        .env_remove("AGENTSTACK_REGISTRY_URL")
        .env_remove("AGENTSTACK_TOKEN")
        .assert()
        .success();

    let json: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["left"]["source_type"], "path");
    assert_eq!(json["right"]["source_type"], "path");
    assert_eq!(json["changed"][0]["path"], "SKILL.md");
    assert_eq!(json["changed_count"], 1);
    assert_eq!(json["is_empty"], false);
}
