//! Integration tests for `agentstack doctor`.
//!
//! Each test is run against an isolated config dir so the user's real
//! AgentStack installation isn't read or modified.

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

fn fresh_env() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let token_file = tmp.child("tokens.json");
    (
        tmp,
        cfg.path().to_path_buf(),
        cache.path().to_path_buf(),
        token_file.path().to_path_buf(),
    )
}

fn cmd(cfg: &std::path::Path, cache: &std::path::Path, token_file: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("agentstack").unwrap();
    c.env("AGENTSTACK_CONFIG_DIR", cfg)
        .env("AGENTSTACK_CACHE_DIR", cache)
        .env("AGENTSTACK_TOKEN_FILE", token_file)
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .env_remove("AGENTSTACK_REGISTRY_URL");
    c
}

fn cmd_with_home(
    cfg: &std::path::Path,
    cache: &std::path::Path,
    token_file: &std::path::Path,
    home: &std::path::Path,
) -> Command {
    let mut c = cmd(cfg, cache, token_file);
    c.env("HOME", home);
    c
}

fn seed_token(token_file: &std::path::Path, registry_url: &str, token: &str) {
    let account = format!(
        "registry:{}/v1/:account:default",
        registry_url.trim_end_matches('/')
    );
    std::fs::write(
        token_file,
        serde_json::json!({ account: token }).to_string(),
    )
    .unwrap();
}

#[test]
fn doctor_runs_in_clean_temp_config() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    cmd(&cfg, &cache, &token_file)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("cli version"))
        .stdout(predicate::str::contains("config dir"))
        .stdout(predicate::str::contains("cache dir"))
        .stdout(predicate::str::contains("registry url"))
        .stdout(predicate::str::contains("auth token"))
        .stdout(predicate::str::contains("target: claude-code"))
        .stdout(predicate::str::contains("target: codex"))
        .stdout(predicate::str::contains("target: local"))
        .stdout(predicate::str::contains("summary:"))
        .stdout(predicate::str::contains("agentstack auth login").not());
}

#[test]
fn doctor_reports_token_presence_without_printing_token() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    cmd(&cfg, &cache, &token_file)
        .args(["registry", "use", "https://registry.example.com"])
        .assert()
        .success();
    seed_token(
        &token_file,
        "https://registry.example.com",
        "supersecretdoctortoken",
    );

    cmd(&cfg, &cache, &token_file)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("present (from file)"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("agentstack auth whoami"))
        .stdout(predicate::str::contains("***oken").not())
        .stdout(predicate::str::contains("supersecretdoctortoken").not());
}

#[test]
fn doctor_emits_parseable_json() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    let output = cmd(&cfg, &cache, &token_file)
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("doctor --json must be parseable");
    assert!(json["cli_version"].is_string());
    let checks = json["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty(), "should have at least one check");
    for c in checks {
        assert!(c["name"].is_string());
        assert!(c["status"].is_string());
        assert!(c["detail"].is_string());
    }
    let summary = &json["summary"];
    assert!(summary["ok"].as_u64().is_some());
    assert!(summary["warn"].as_u64().is_some());
    assert!(summary["fail"].as_u64().is_some());
}

#[test]
fn doctor_json_does_not_attach_fix_commands_to_ok_checks() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    let output = cmd(&cfg, &cache, &token_file)
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("doctor --json must be parseable");
    for check in json["checks"].as_array().expect("checks array") {
        if check["status"] == "ok" {
            assert!(
                check.get("fix_command").is_none() || check["fix_command"].is_null(),
                "ok check should not include fix_command: {check}"
            );
        }
    }
}

#[test]
fn doctor_reports_registry_url_when_configured() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    cmd(&cfg, &cache, &token_file)
        .args(["registry", "use", "https://registry.example.com"])
        .assert()
        .success();

    cmd(&cfg, &cache, &token_file)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("https://registry.example.com"));
}

#[test]
fn doctor_reports_registry_url_env_override_without_token_material() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    let mut c = cmd(&cfg, &cache, &token_file);
    c.env(
        "AGENTSTACK_REGISTRY_URL",
        "https://env.registry.example.com",
    )
    .env("AGENTSTACK_TOKEN", "doctorenvsecret1234")
    .arg("doctor")
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "https://env.registry.example.com (from AGENTSTACK_REGISTRY_URL)",
    ))
    .stdout(predicate::str::contains(
        "present (from AGENTSTACK_TOKEN env var)",
    ))
    .stdout(predicate::str::contains("doctorenvsecret1234").not())
    .stdout(predicate::str::contains("***1234").not());
}

#[test]
fn doctor_reports_token_path_without_token_material() {
    let (tmp, cfg, cache, token_file) = fresh_env();
    let token_path = tmp.child("agentstack-token");
    token_path.write_str("doctorpathsecret1234\n").unwrap();
    let mut c = cmd(&cfg, &cache, &token_file);
    c.env(
        "AGENTSTACK_REGISTRY_URL",
        "https://env.registry.example.com",
    )
    .env("AGENTSTACK_TOKEN_PATH", token_path.path())
    .arg("doctor")
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "present (from AGENTSTACK_TOKEN_PATH file)",
    ))
    .stdout(predicate::str::contains("env token path"))
    .stdout(predicate::str::contains(
        token_path.path().display().to_string(),
    ))
    .stdout(predicate::str::contains("doctorpathsecret1234").not())
    .stdout(predicate::str::contains("***1234").not());
}

#[test]
fn doctor_describes_creatable_target_as_not_yet_created() {
    // When a default target's destination doesn't exist but the parent is
    // writable, doctor must not say "not writable" — that scares first-time
    // users into thinking their box is broken. It should suggest setup.
    let (tmp, cfg, cache, token_file) = fresh_env();
    let target_root = tmp.child("dest");
    target_root.create_dir_all().unwrap();
    let dest = target_root.path().join("skills");
    cmd(&cfg, &cache, &token_file)
        .args(["target", "set", "local", "--path"])
        .arg(&dest)
        .assert()
        .success();

    cmd(&cfg, &cache, &token_file)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("not yet created"))
        .stdout(predicate::str::contains(
            "run `agentstack target setup local --yes`",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack target setup local --yes",
        ))
        .stdout(predicate::str::contains("agentstack target setup --yes").not())
        .stdout(predicate::str::contains("parent is not writable").not());
}

#[test]
fn doctor_warns_for_unconfigured_user_level_targets_even_when_default_dirs_exist() {
    let (tmp, cfg, cache, token_file) = fresh_env();
    let home = tmp.child("home");
    home.child(".claude/skills").create_dir_all().unwrap();
    home.child(".codex/skills").create_dir_all().unwrap();

    cmd_with_home(&cfg, &cache, &token_file, home.path())
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("target: claude-code"))
        .stdout(predicate::str::contains(
            "run `agentstack target setup claude-code --yes` before user-level installs",
        ))
        .stdout(predicate::str::contains("target: codex"))
        .stdout(predicate::str::contains(
            "run `agentstack target setup codex --yes` before user-level installs",
        ))
        .stdout(predicate::str::contains("next:").not());
}

#[test]
fn doctor_unconfigured_user_level_targets_are_ok_when_absent() {
    let (tmp, cfg, cache, token_file) = fresh_env();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let output = cmd_with_home(&cfg, &cache, &token_file, home.path())
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("doctor --json must be parseable");
    for name in ["target: claude-code", "target: codex"] {
        let check = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(check["status"], "ok", "{name}: {check}");
        assert!(
            check.get("fix_command").is_none() || check["fix_command"].is_null(),
            "{name} should not prescribe setup: {check}"
        );
    }
}

#[test]
fn doctor_fresh_home_treats_nested_cache_as_creatable() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let output = Command::cargo_bin("agentstack")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .env_remove("AGENTSTACK_TOKEN_FILE")
        .env_remove("AGENTSTACK_ALLOW_TOKEN_FILE")
        .env_remove("AGENTSTACK_REGISTRY_URL")
        .env_remove("AGENTSTACK_CONFIG_DIR")
        .env_remove("AGENTSTACK_CACHE_DIR")
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("doctor --json must be parseable");
    for name in ["config dir", "cache dir"] {
        let check = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(check["status"], "ok", "{name}: {check}");
        assert!(
            check["detail"]
                .as_str()
                .unwrap()
                .contains("will be created on first use"),
            "{name}: {check}"
        );
    }
    assert_eq!(json["summary"]["fail"], 0);
}

#[test]
fn doctor_fresh_install_does_not_prescribe_next_commands() {
    let (tmp, cfg, cache, _token_file) = fresh_env();
    let home = tmp.child("home");
    home.child(".claude/skills").create_dir_all().unwrap();
    home.child(".codex/skills").create_dir_all().unwrap();

    let mut c = Command::cargo_bin("agentstack").unwrap();
    c.env("AGENTSTACK_CONFIG_DIR", &cfg)
        .env("AGENTSTACK_CACHE_DIR", &cache)
        .env("HOME", home.path())
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .env_remove("AGENTSTACK_TOKEN_FILE")
        .env_remove("AGENTSTACK_ALLOW_TOKEN_FILE")
        .env_remove("AGENTSTACK_REGISTRY_URL")
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("not logged in"))
        .stdout(predicate::str::contains("agentstack auth login").not())
        .stdout(predicate::str::contains("next:").not());
}

#[test]
fn doctor_checks_all_targets_even_when_one_target_is_configured() {
    let (tmp, cfg, cache, token_file) = fresh_env();
    let local = tmp.child("local-skills");
    local.create_dir_all().unwrap();
    cmd(&cfg, &cache, &token_file)
        .args(["target", "set", "local", "--path"])
        .arg(local.path())
        .assert()
        .success();

    let output = cmd(&cfg, &cache, &token_file)
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("doctor --json must be parseable");
    let targets = json["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|check| check["name"].as_str())
        .filter(|name| name.starts_with("target: "))
        .collect::<Vec<_>>();
    assert!(targets.contains(&"target: claude-code"));
    assert!(targets.contains(&"target: codex"));
    assert!(targets.contains(&"target: repo-claude-code"));
    assert!(targets.contains(&"target: repo-codex"));
    assert!(targets.contains(&"target: local"));
}

#[test]
fn doctor_warns_when_token_file_override_present() {
    let (_tmp, cfg, cache, token_file) = fresh_env();
    cmd(&cfg, &cache, &token_file)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN_FILE"))
        .stderr(predicate::str::contains("AGENTSTACK_TOKEN_FILE is honored"));
}
