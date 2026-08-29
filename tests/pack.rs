use std::fs;
use std::path::Path;

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;

const SKILL_MD: &str = "\
---
name: my-skill
description: Use when foo happens
---

# Purpose

Body.

# When to Use

# Instructions

# Output

# Boundaries
";

fn make_skill(parent: &TempDir) -> ChildPath {
    let target = parent.child("my-skill");
    target.create_dir_all().unwrap();
    target.child("SKILL.md").write_str(SKILL_MD).unwrap();
    for sub in ["references", "examples", "assets", "scripts", "platform"] {
        target.child(sub).create_dir_all().unwrap();
    }
    target
        .child("references/notes.md")
        .write_str("note body")
        .unwrap();
    target
}

fn cache_env(cache_dir: &std::path::Path) -> [(&str, &std::path::Path); 1] {
    [("AGENTSTACK_CACHE_DIR", cache_dir)]
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
fn pack_writes_archive_and_prints_hash() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let out = tmp.child("my-skill.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("packed "))
        .stdout(predicate::str::contains("sha256:"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("agentstack skill install "))
        .stdout(predicate::str::contains("--target local"))
        .stdout(predicate::str::contains("agentstack skill unpack "))
        .stdout(predicate::str::contains("--out ./skills"));

    out.assert(predicate::path::is_file());
    let bytes = fs::read(out.path()).unwrap();
    assert!(
        bytes.starts_with(&[0x1f, 0x8b]),
        "expected gzip magic header"
    );
}

#[test]
fn pack_quiet_writes_archive_without_next_steps() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let out = tmp.child("quiet-skill.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "--quiet",
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("next:").not());

    out.assert(predicate::path::is_file());
}

#[test]
fn skill_pack_rejects_removed_output_alias() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let out = tmp.child("my-skill-alias.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--output",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument '--output'"))
        .stderr(predicate::str::contains("--out"));

    out.assert(predicate::path::missing());
}

#[test]
fn pack_warns_on_lint_warnings_but_writes_archive() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    skill
        .child("SKILL.md")
        .write_str(
            "\
---
name: my-skill
description: Use when foo happens
---

# Purpose

TODO: replace scaffold text.

# When to Use

# Instructions

# Output

# Boundaries
",
        )
        .unwrap();
    let cache = tmp.child("cache");
    let out = tmp.child("my-skill.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("packed "))
        .stderr(predicate::str::contains("warning: archive written with"))
        .stderr(predicate::str::contains("agentstack skill lint"));

    out.assert(predicate::path::is_file());
}

#[cfg(unix)]
#[test]
fn pack_warns_on_excluded_symlinks_instead_of_dropping_silently() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    // A symlink inside the skill tree cannot be packaged; the author must be
    // told it was excluded rather than discovering a missing file later.
    std::os::unix::fs::symlink(
        Path::new("notes.md"),
        skill.child("references/linked.md").path(),
    )
    .unwrap();
    let cache = tmp.child("cache");
    let out = tmp.child("my-skill.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains("symlink").and(predicate::str::contains("excluded")))
        .stderr(predicate::str::contains("references/linked.md"));

    out.assert(predicate::path::is_file());

    // The same exclusion must surface in JSON for CI automation.
    let json_out = tmp.child("my-skill-json.tar.gz");
    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "--json",
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            json_out.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("skipped_symlinks"))
        .stdout(predicate::str::contains("references/linked.md"));
}

#[test]
fn pack_default_out_uses_skill_name() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let cwd = tmp.child("workdir");
    cwd.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .current_dir(cwd.path())
        .args(["skill", "pack", skill.path().to_str().unwrap()])
        .assert()
        .success();

    cwd.child("my-skill.tar.gz")
        .assert(predicate::path::is_file());
}

#[test]
fn pack_excludes_junk_files() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    skill.child(".git").create_dir_all().unwrap();
    skill
        .child(".git/HEAD")
        .write_str("ref: refs/heads/main")
        .unwrap();
    skill.child("target").create_dir_all().unwrap();
    skill.child("target/debug.log").write_str("debug").unwrap();
    skill.child("node_modules/foo").create_dir_all().unwrap();
    skill.child(".DS_Store").write_str("junk").unwrap();
    skill.child(".env").write_str("SECRET=1").unwrap();
    skill
        .child("references/private.pem")
        .write_str("-----BEGIN PRIVATE KEY-----")
        .unwrap();

    let cache = tmp.child("cache");
    let out = tmp.child("pkg.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let extract_parent = tmp.child("extract");
    let extract = extract_parent.child("my-skill");
    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "unpack",
            out.path().to_str().unwrap(),
            "--out",
            extract_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Excluded entries must not appear after roundtrip.
    extract.child(".git").assert(predicate::path::missing());
    extract.child("target").assert(predicate::path::missing());
    extract
        .child("node_modules")
        .assert(predicate::path::missing());
    extract
        .child(".DS_Store")
        .assert(predicate::path::missing());
    extract.child(".env").assert(predicate::path::missing());
    extract
        .child("references/private.pem")
        .assert(predicate::path::missing());

    // Included content survives.
    extract.child("SKILL.md").assert(predicate::path::is_file());
    extract
        .child("references/notes.md")
        .assert(predicate::path::is_file());
}

#[test]
fn pack_skips_hidden_and_secret_files_but_keeps_normal_markdown_references() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    for file in [
        ".npmrc",
        ".netrc",
        ".pypirc",
        "tokens.json",
        "kubeconfig",
        "production.env",
        "client.p12",
        "client.pfx",
        "keystore.jks",
    ] {
        skill
            .child("references")
            .child(file)
            .write_str("secret-ish")
            .unwrap();
    }
    skill
        .child("references/credentials-guide.md")
        .write_str("how to configure credentials")
        .unwrap();

    let cache = tmp.child("cache");
    let out = tmp.child("pkg.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let extract_parent = tmp.child("extract");
    let extract = extract_parent.child("my-skill");
    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "unpack",
            out.path().to_str().unwrap(),
            "--out",
            extract_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    for file in [
        ".npmrc",
        ".netrc",
        ".pypirc",
        "tokens.json",
        "kubeconfig",
        "production.env",
        "client.p12",
        "client.pfx",
        "keystore.jks",
    ] {
        extract
            .child("references")
            .child(file)
            .assert(predicate::path::missing());
    }
    extract
        .child("references/credentials-guide.md")
        .assert(predicate::path::is_file());
}

#[test]
fn pack_includes_arbitrary_support_files_and_directories() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    skill
        .child("reference.md")
        .write_str("# reference")
        .unwrap();
    skill
        .child("LICENSE.txt")
        .write_str("Copyright example")
        .unwrap();
    skill
        .child("templates/template.md")
        .write_str("# template")
        .unwrap();
    skill
        .child("agents/openai.yaml")
        .write_str("name: my-skill")
        .unwrap();
    let cache = tmp.child("cache");
    let out = tmp.child("pkg.tar.gz");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let extract_parent = tmp.child("extract");
    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "unpack",
            out.path().to_str().unwrap(),
            "--out",
            extract_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let extract = extract_parent.child("my-skill");
    extract
        .child("reference.md")
        .assert(predicate::path::is_file());
    extract
        .child("LICENSE.txt")
        .assert(predicate::path::is_file());
    extract
        .child("templates/template.md")
        .assert(predicate::path::is_file());
    extract
        .child("agents/openai.yaml")
        .assert(predicate::path::is_file());
}

#[test]
fn pack_rejects_name_directory_mismatch() {
    let tmp = TempDir::new().unwrap();
    let skill = tmp.child("wrong-dir");
    skill.create_dir_all().unwrap();
    skill.child("SKILL.md").write_str(SKILL_MD).unwrap();
    let cache = tmp.child("cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            tmp.child("pkg.tar.gz").path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("name"))
        .stderr(predicate::str::contains("wrong-dir"));
}

#[test]
fn pack_refuses_overwrite_without_force() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");
    let out = tmp.child("pkg.tar.gz");
    out.write_str("existing").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--force"));

    // The file is unchanged.
    assert_eq!(fs::read_to_string(out.path()).unwrap(), "existing");

    // With --force, the pack succeeds.
    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
            "--force",
        ])
        .assert()
        .success();
    let bytes = fs::read(out.path()).unwrap();
    assert!(bytes.starts_with(&[0x1f, 0x8b]));
    assert_no_agentstack_temps_under(tmp.path());
}

#[test]
fn pack_hash_is_stable_for_same_content() {
    let tmp = TempDir::new().unwrap();
    let skill = make_skill(&tmp);
    let cache = tmp.child("cache");

    let out_a = tmp.child("a.tar.gz");
    let out_b = tmp.child("b.tar.gz");

    for out in [&out_a, &out_b] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .envs(cache_env(cache.path()))
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

    let bytes_a = fs::read(out_a.path()).unwrap();
    let bytes_b = fs::read(out_b.path()).unwrap();
    assert_eq!(
        bytes_a, bytes_b,
        "packing identical content should produce byte-identical archives"
    );
}

#[test]
fn pack_rejects_invalid_skill() {
    let tmp = TempDir::new().unwrap();
    let bogus = tmp.child("not-a-skill");
    bogus.create_dir_all().unwrap();
    bogus.child("README.md").write_str("# nope").unwrap();
    let cache = tmp.child("cache");

    Command::cargo_bin("agentstack")
        .unwrap()
        .envs(cache_env(cache.path()))
        .args([
            "skill",
            "pack",
            bogus.path().to_str().unwrap(),
            "--out",
            tmp.child("pkg.tar.gz").path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid skill"));
}
