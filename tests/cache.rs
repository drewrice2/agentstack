use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn make_skill(parent: &TempDir, name: &str, description: &str) -> ChildPath {
    let body = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
    );
    let target = parent.child(name);
    target.create_dir_all().unwrap();
    target.child("SKILL.md").write_str(&body).unwrap();
    target
}

fn pack(skill: &ChildPath, out: &ChildPath, cache: &ChildPath) {
    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();
}

fn cache_entry_dir(cache: &ChildPath, name: &str) -> PathBuf {
    let skill_dir = cache.path().join("skills").join(name);
    let entries = fs::read_dir(&skill_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "expected one cached version for {name}");
    entries.into_iter().next().unwrap()
}

fn assert_no_agentstack_temps_under(root: &Path) {
    let mut found = Vec::new();
    collect_agentstack_temps(root, &mut found);
    assert!(found.is_empty(), "leftover temp paths: {found:?}");
}

fn collect_agentstack_temps(root: &Path, found: &mut Vec<String>) {
    if !root.exists() {
        return;
    }

    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with(".agentstack-"))
            .unwrap_or(false)
        {
            found.push(path.display().to_string());
        }
        if path.is_dir() {
            collect_agentstack_temps(&path, found);
        }
    }
}

#[test]
fn cache_path_prints_override() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("my-cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(cache.path().to_str().unwrap()));
}

#[test]
fn cache_list_is_empty_when_nothing_packed() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cache is empty"))
        .stdout(predicate::str::contains("next: agentstack skill pack"));
}

#[test]
fn pack_populates_cache_and_list_shows_entry() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");

    let skill_a = make_skill(&tmp, "alpha", "use when alpha");
    let skill_b = make_skill(&tmp, "beta", "use when beta");
    let pkg_a = tmp.child("alpha.tar.gz");
    let pkg_b = tmp.child("beta.tar.gz");
    pack(&skill_a, &pkg_a, &cache);
    pack(&skill_b, &pkg_b, &cache);

    for name in ["alpha", "beta"] {
        let entry_dir = cache_entry_dir(&cache, name);
        assert!(entry_dir.join("package.tar.gz").is_file());
        assert!(entry_dir.join("manifest.json").is_file());
    }
    assert_no_agentstack_temps_under(cache.path());

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("beta"))
        .stdout(predicate::str::contains("local-dev"));
}

#[test]
fn cache_remove_drops_skill() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");

    let skill = make_skill(&tmp, "removable", "use when removable");
    let pkg = tmp.child("removable.tar.gz");
    pack(&skill, &pkg, &cache);

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "remove", "removable", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed `removable`"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cache is empty"))
        .stdout(predicate::str::contains("next: agentstack skill pack"));
}

#[test]
fn cache_remove_unknown_skill_errors() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "remove", "ghost", "--force"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no cached skill"));
}

#[test]
fn cache_remove_unknown_skill_errors_before_force_prompt() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "remove", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no cached skill"))
        .stderr(predicate::str::contains("--force").not());
}

#[test]
fn cache_remove_without_force_refuses_non_interactively_for_existing_skill() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");
    let skill = make_skill(&tmp, "ghost", "use when ghost");
    let pkg = tmp.child("ghost.tar.gz");
    pack(&skill, &pkg, &cache);

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "remove", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));
}

#[test]
fn pack_with_no_cache_skips_cache_entry() {
    let tmp = TempDir::new().unwrap();
    let cache = tmp.child("cache");
    let skill = make_skill(&tmp, "uncached", "use when uncached");
    let pkg = tmp.child("uncached.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            pkg.path().to_str().unwrap(),
            "--no-cache",
        ])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["cache", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cache is empty"))
        .stdout(predicate::str::contains("next: agentstack skill pack"));
}

#[test]
fn cache_path_default_includes_agentstack() {
    // Without an override, the cache path should still point somewhere
    // recognizable as AgentStack's own cache directory.
    Command::cargo_bin("agentstack")
        .unwrap()
        .env_remove("AGENTSTACK_CACHE_DIR")
        .args(["cache", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agentstack").or(predicate::str::contains("AgentStack")));
}
