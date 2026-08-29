//! Tests that the global `--json` flag produces parseable JSON for every
//! command that documents JSON output. The point isn't to lock in the exact
//! shape (the inspect tests already do that for the most-used command), but
//! to make sure the JSON path doesn't silently regress for the rest.

use std::collections::BTreeSet;
use std::io::{Read, Write};

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use serde_json::Value;

fn write_skill(dir: &ChildPath, name: &str, description: &str) {
    dir.create_dir_all().unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    dir.child("SKILL.md").write_str(&body).unwrap();
}

fn cmd(cfg: &std::path::Path, cache: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("agentstack").unwrap();
    c.env("AGENTSTACK_CONFIG_DIR", cfg)
        .env("AGENTSTACK_CACHE_DIR", cache)
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_REGISTRY_URL");
    c
}

fn parse(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("expected parseable JSON on stdout")
}

fn parse_stderr(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("expected parseable JSON on stderr")
}

fn whoami_body() -> &'static str {
    r#"{
        "user": "pilot@example.com",
        "org": "demo",
        "email": "pilot@example.com",
        "name": "Pilot User",
        "server_admin": true,
        "orgs": [
            { "slug": "demo", "name": "Demo", "role": "org_admin" }
        ]
    }"#
}

fn whoami_server_n(count: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());
            let body = whoami_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
        requests
    });
    (url, handle)
}

fn assert_required_top_level_keys(json: &Value, required: &[&str]) {
    let actual: BTreeSet<&str> = json
        .as_object()
        .expect("top-level JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    for key in required {
        assert!(
            actual.contains(key),
            "missing required key `{key}` in {actual:?}"
        );
    }
}

fn set_local_target(cfg: &std::path::Path, cache: &std::path::Path, target: &std::path::Path) {
    cmd(cfg, cache)
        .args(["target", "set", "local", "--path", target.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn validate_json_success_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("valid-json");
    write_skill(&skill, "valid-json", "use when validating json");

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "validate",
            skill.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json = parse(&assert.get_output().stdout);

    assert_required_top_level_keys(&json, &["ok", "path", "name", "description", "errors"]);
    assert_eq!(json["ok"], Value::Bool(true));
    assert!(json["path"].is_string());
    assert_eq!(json["name"].as_str(), Some("valid-json"));
    assert_eq!(
        json["description"].as_str(),
        Some("use when validating json")
    );
    assert!(json["errors"].as_array().expect("errors array").is_empty());
}

#[test]
fn validate_json_failure_contract_keys_and_types_without_manifest() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("missing-manifest");
    skill.create_dir_all().unwrap();

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "validate",
            skill.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
    let stderr = parse_stderr(&assert.get_output().stderr);
    assert_eq!(stderr["error"]["code"].as_str(), Some("missing_skill_md"));
    assert_eq!(stderr["error"]["action"].as_str(), Some("validate_skill"));
    assert!(
        assert.get_output().stdout.is_empty(),
        "validate --json failures should emit one JSON error envelope on stderr only"
    );
}

#[test]
fn validate_json_failure_error_code_uses_first_validation_error() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("wrong-name");
    write_skill(&skill, "other-name", "use when validating json");

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "validate",
            skill.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
    let stderr = parse_stderr(&assert.get_output().stderr);
    assert_eq!(stderr["error"]["code"].as_str(), Some("name_mismatch"));
    assert_eq!(
        stderr["error"]["resource"].as_str(),
        Some(skill.path().to_str().unwrap())
    );
    assert!(
        assert.get_output().stdout.is_empty(),
        "validate --json failures should emit one JSON error envelope on stderr only"
    );
}

#[test]
fn init_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("new-skill");

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "init",
            skill.path().to_str().unwrap(),
            "--name",
            "new-skill",
            "--description",
            "use when initializing json",
        ])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json = parse(&assert.get_output().stdout);

    assert_required_top_level_keys(&json, &["name", "path", "skill_md", "subdirs"]);
    assert_eq!(json["name"].as_str(), Some("new-skill"));
    assert!(json["path"].is_string());
    assert!(json["skill_md"].is_string());
    let subdirs = json["subdirs"].as_array().expect("subdirs array");
    for expected in ["references", "examples", "assets", "scripts", "platform"] {
        assert!(
            subdirs.iter().any(|s| s.as_str() == Some(expected)),
            "expected subdir `{expected}` in {subdirs:?}"
        );
    }
}

#[test]
fn inspect_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("inspectable");
    write_skill(&skill, "inspectable", "use when inspecting json");

    let assert = cmd(cfg.path(), cache.path())
        .args(["--json", "skill", "inspect", skill.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json = parse(&assert.get_output().stdout);

    assert_required_top_level_keys(
        &json,
        &[
            "name",
            "description",
            "path",
            "skill_md",
            "directories",
            "unknown_files",
            "errors",
            "warnings",
            "package_hash",
        ],
    );
    assert_eq!(json["name"].as_str(), Some("inspectable"));
    assert_eq!(
        json["description"].as_str(),
        Some("use when inspecting json")
    );
    assert!(json["path"].is_string());
    assert!(json["skill_md"].is_object());
    assert!(json["directories"].is_object());
    assert!(json["unknown_files"].is_array());
    assert!(json["errors"].is_array());
    assert!(json["warnings"].is_array());
    assert!(json["package_hash"].is_string() || json["package_hash"].is_null());
}

#[test]
fn lint_json_success_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("lintable");
    write_skill(&skill, "lintable", "use when linting json");
    skill.child("examples").create_dir_all().unwrap();
    skill.child("references").create_dir_all().unwrap();

    let assert = cmd(cfg.path(), cache.path())
        .args(["--json", "skill", "lint", skill.path().to_str().unwrap()])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json = parse(&assert.get_output().stdout);

    assert_required_top_level_keys(&json, &["ok", "path", "validation_errors", "warnings"]);
    assert_eq!(json["ok"], Value::Bool(true));
    assert!(json["path"].is_string());
    assert!(
        json["validation_errors"]
            .as_array()
            .expect("validation_errors array")
            .is_empty()
    );
    assert!(
        json["warnings"]
            .as_array()
            .expect("warnings array")
            .is_empty()
    );
}

#[test]
fn list_local_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    write_skill(&tmp.child("alpha"), "alpha", "use when alpha");
    write_skill(&tmp.child("beta"), "beta", "use when beta");

    let out = cmd(cfg.path(), cache.path())
        .current_dir(tmp.path())
        .args(["--json", "skill", "scan"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["skills"]);
    let skills = json["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 2);
    let names: Vec<&str> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

#[test]
fn skill_scan_json_empty_state_has_concrete_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let empty = tmp.child("empty");
    empty.create_dir_all().unwrap();

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "skill", "scan", empty.path().to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["skills", "empty_message", "next_command"]);
    assert!(json["skills"].as_array().expect("skills array").is_empty());
    assert!(
        json["empty_message"]
            .as_str()
            .unwrap()
            .contains("no skills found")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some(
            "agentstack skill init my-skill --name my-skill --description \"Use when reviewing PRs\""
        )
    );
    assert!(!json["next_command"].as_str().unwrap().contains('<'));
}

#[test]
fn cache_list_json_when_empty_emits_empty_entries() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "cache", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["root", "entries", "empty_message", "next_command"]);
    assert!(json["root"].is_string());
    let entries = json["entries"].as_array().expect("entries array");
    assert!(entries.is_empty());
    assert!(
        json["empty_message"]
            .as_str()
            .unwrap()
            .contains("cache is empty")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill pack ./my-skill")
    );
}

#[test]
fn pack_and_unpack_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("packable-json");
    let package = tmp.child("packable-json.tar.gz");
    let unpack_parent = tmp.child("unpacked");
    write_skill(&skill, "packable-json", "use when packing json");

    let packed = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            package.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(packed.get_output().stderr.is_empty());
    let json = parse(&packed.get_output().stdout);
    assert_required_top_level_keys(
        &json,
        &[
            "name",
            "version",
            "path",
            "files",
            "size_bytes",
            "sha256",
            "cached_at",
            "next_command",
        ],
    );
    assert_eq!(json["name"].as_str(), Some("packable-json"));
    assert!(json["path"].is_string());
    assert!(json["cached_at"].is_string());
    assert!(
        json["next_command"]
            .as_str()
            .unwrap()
            .starts_with("agentstack skill unpack ")
    );
    assert!(
        json["next_command"]
            .as_str()
            .unwrap()
            .contains(" --out ./skills")
    );
    assert!(!json["next_command"].as_str().unwrap().contains('<'));

    let unpacked = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "unpack",
            package.path().to_str().unwrap(),
            "--out",
            unpack_parent.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(unpacked.get_output().stderr.is_empty());
    let json = parse(&unpacked.get_output().stdout);
    assert_required_top_level_keys(&json, &["name", "out", "files", "sha256"]);
    assert_eq!(json["name"].as_str(), Some("packable-json"));
    assert!(json["out"].is_string());
    assert!(json["files"].as_u64().unwrap() > 0);
    assert!(json["sha256"].is_string());
}

#[test]
fn config_show_json_emits_nested_shape() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    cmd(cfg.path(), cache.path())
        .args(["registry", "use", "https://r.example.com"])
        .assert()
        .success();

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["path", "config"]);
    assert!(json["path"].is_string());
    let cfg_obj = &json["config"];
    assert_eq!(
        cfg_obj["registry"]["url"].as_str(),
        Some("https://r.example.com")
    );
}

#[test]
fn targets_list_json_lists_known_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "target", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["targets"]);
    let targets = json["targets"].as_array().expect("targets array");
    let names: Vec<&str> = targets
        .iter()
        .map(|t| t["target"].as_str().unwrap())
        .collect();
    for expected in [
        "claude-code",
        "codex",
        "repo-claude-code",
        "repo-codex",
        "local",
    ] {
        assert!(
            names.contains(&expected),
            "expected target `{expected}` in {names:?}"
        );
    }
}

#[test]
fn targets_path_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("target-path");

    set_local_target(cfg.path(), cache.path(), target.path());

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "target", "path", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["target", "path", "source"]);
    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["path"].as_str(), Some(target.path().to_str().unwrap()));
    assert_eq!(json["source"].as_str(), Some("override"));
}

#[test]
fn targets_detect_json_lists_readiness_fields() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let out = cmd(cfg.path(), cache.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    let targets = json["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 5);
    for row in targets {
        assert!(row["target"].is_string());
        assert!(row["description"].is_string());
        assert!(row["configured"].is_boolean());
        assert!(row["source"].is_string());
        assert!(row["exists"].is_boolean());
        assert!(row["is_dir"].is_boolean());
        assert!(row["writable"].is_boolean());
        assert!(row["creatable"].is_boolean());
        assert!(row["usable"].is_boolean());
        assert!(row["path"].is_string() || row["path"].is_null());
        assert!(row["fix_command"].is_string() || row["fix_command"].is_null());
    }
}

#[test]
fn registry_get_json_when_unset_reports_default() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "registry", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(&json, &["url", "source"]);
    assert_eq!(json["url"].as_str(), Some("https://registry.agentstack.gg"));
    assert_eq!(json["source"].as_str(), Some("default"));
}

#[test]
fn registry_set_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let out = cmd(cfg.path(), cache.path())
        .args(["--json", "registry", "use", "https://registry.example.com"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(
        &json,
        &["url", "config", "active_source", "saved_url_active"],
    );
    assert_eq!(json["url"].as_str(), Some("https://registry.example.com"));
    assert!(json["config"].is_string());
    assert_eq!(json["active_source"].as_str(), Some("config"));
    assert_eq!(json["saved_url_active"], Value::Bool(true));
}

#[test]
fn registry_set_json_reports_env_override_without_echoing_env_url() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let out = cmd(cfg.path(), cache.path())
        .env(
            "AGENTSTACK_REGISTRY_URL",
            "https://env.registry.example.com",
        )
        .args(["--json", "registry", "use", "https://registry.example.com"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json = parse(&out);
    assert_required_top_level_keys(
        &json,
        &["url", "config", "active_source", "saved_url_active"],
    );
    assert_eq!(json["url"].as_str(), Some("https://registry.example.com"));
    assert_eq!(
        json["active_source"].as_str(),
        Some("AGENTSTACK_REGISTRY_URL")
    );
    assert_eq!(json["saved_url_active"], Value::Bool(false));
    assert!(
        !out.windows(b"https://env.registry.example.com".len())
            .any(|window| window == b"https://env.registry.example.com")
    );
}

#[test]
fn config_set_and_unset_target_json_emit_only_json() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("target");

    let set = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    assert!(set.get_output().stderr.is_empty());
    let json = parse(&set.get_output().stdout);
    assert_required_top_level_keys(&json, &["target", "path", "config"]);
    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["path"].as_str(), Some(target.path().to_str().unwrap()));

    let unset = cmd(cfg.path(), cache.path())
        .args(["--json", "target", "unset", "local"])
        .assert()
        .success();
    assert!(unset.get_output().stderr.is_empty());
    let json = parse(&unset.get_output().stdout);
    assert_required_top_level_keys(&json, &["target", "removed", "previous", "config"]);
    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["removed"], Value::Bool(true));
    assert_eq!(
        json["previous"].as_str(),
        Some(target.path().to_str().unwrap())
    );
}

#[test]
fn cache_remove_force_json_emits_only_json() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("removable");
    let package = tmp.child("removable.tar.gz");
    write_skill(&skill, "removable", "use when removable");

    cmd(cfg.path(), cache.path())
        .args([
            "skill",
            "pack",
            skill.path().to_str().unwrap(),
            "--out",
            package.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let removed = cmd(cfg.path(), cache.path())
        .args(["--json", "cache", "remove", "removable", "--force"])
        .assert()
        .success();
    assert!(removed.get_output().stderr.is_empty());
    let json = parse(&removed.get_output().stdout);
    assert_eq!(json["name"].as_str(), Some("removable"));
    assert_eq!(json["removed"], Value::Bool(true));
    assert!(json["root"].is_string());
    assert!(json["skills_dir"].is_string());
}

#[test]
fn login_and_logout_json_never_print_token() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let token_file = tmp.child("tokens.json");
    let (url, handle) = whoami_server_n(2);

    cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["registry", "use", &url])
        .assert()
        .success();

    let login = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "login"])
        .write_stdin("jsonstdinsecret1234\n")
        .assert()
        .success();
    assert!(login.get_output().stderr.is_empty());
    let stdout = String::from_utf8(login.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("jsonstdinsecret1234"));
    assert!(!stdout.contains("***1234"));
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["server"].as_str(), Some(url.as_str()));
    assert_eq!(json["replaced_existing_token"], Value::Bool(false));
    assert_eq!(json["email"].as_str(), Some("pilot@example.com"));
    assert!(json.get("token").is_none());

    let reauth = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "login"])
        .write_stdin("jsonreplacementsecret5678\n")
        .assert()
        .success();
    assert!(reauth.get_output().stderr.is_empty());
    let stdout = String::from_utf8(reauth.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("jsonstdinsecret1234"));
    assert!(!stdout.contains("jsonreplacementsecret5678"));
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["replaced_existing_token"], Value::Bool(true));
    assert_eq!(json["email"].as_str(), Some("pilot@example.com"));
    assert!(json.get("token").is_none());
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);

    let logout = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "logout"])
        .assert()
        .success();
    assert!(logout.get_output().stderr.is_empty());
    let json = parse(&logout.get_output().stdout);
    assert_eq!(json["removed"], Value::Bool(true));
}

#[test]
fn login_json_token_stdin_emits_clean_json_only() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let token_file = tmp.child("tokens.json");
    let (url, handle) = whoami_server_n(1);

    cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["registry", "use", &url])
        .assert()
        .success();

    let login = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "login", "--token-stdin"])
        .write_stdin("jsonflagsecret9876\n")
        .assert()
        .success();
    assert!(login.get_output().stderr.is_empty());
    let stdout = String::from_utf8(login.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("jsonflagsecret9876"));
    assert!(!stdout.contains("***9876"));
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["server"].as_str(), Some(url.as_str()));
    assert_eq!(json["email"].as_str(), Some("pilot@example.com"));
    assert_eq!(json["next_command"].as_str(), Some("agentstack skill list"));
    assert!(json.get("next_command_template").is_none());
    assert!(json.get("token").is_none());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
}

#[test]
fn login_rejects_missing_registry_json_without_leaking_secret_input() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let token_file = tmp.child("tokens.json");

    let login = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "login"])
        .write_stdin("jsonbadurlsecret1357\n")
        .assert()
        .failure();

    assert!(login.get_output().stdout.is_empty());
    let stderr = String::from_utf8(login.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains("jsonbadurlsecret1357"));
    assert!(!stderr.contains("***1357"));
}

#[test]
fn install_json_emits_only_success_object() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("install-target");
    let skill = tmp.child("installable");
    write_skill(&skill, "installable", "use when installable");

    set_local_target(cfg.path(), cache.path(), target.path());

    let installed = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();
    assert!(installed.get_output().stderr.is_empty());
    let json = parse(&installed.get_output().stdout);
    assert_required_top_level_keys(
        &json,
        &[
            "name",
            "kind",
            "operation",
            "resource",
            "installed_as",
            "target",
            "target_source",
            "destination",
            "source_type",
            "source_ref",
            "registry_url",
            "org",
            "version",
            "hash",
            "hash_kind",
            "receipt",
            "cache_package",
            "files",
            "overwrote",
            "overlay",
            "platform_warning",
            "warnings",
            "next_commands",
        ],
    );
    assert_eq!(json["name"].as_str(), Some("installable"));
    assert_eq!(json["overlay"], Value::Null);
    assert_eq!(json["platform_warning"], Value::Null);
    assert_eq!(json["kind"].as_str(), Some("skill_install"));
    assert_eq!(json["operation"].as_str(), Some("install"));
    assert_eq!(json["resource"].as_str(), Some("installable"));
    assert_eq!(json["target"].as_str(), Some("local"));
    assert_eq!(json["hash_kind"].as_str(), Some("install_tree"));
    assert_eq!(json["overwrote"], Value::Bool(false));
    assert!(
        json["destination"]
            .as_str()
            .unwrap()
            .ends_with("installable")
    );
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill show installable --target local")
    );
}

#[test]
fn install_json_validation_failure_uses_first_validation_code() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("install-target");
    let skill = tmp.child("missing-skill-md");
    skill.create_dir_all().unwrap();
    set_local_target(cfg.path(), cache.path(), target.path());

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = parse_stderr(&assert.get_output().stderr);
    assert_eq!(stderr["error"]["code"].as_str(), Some("missing_skill_md"));
    assert_eq!(stderr["error"]["action"].as_str(), Some("validate_skill"));
}

#[test]
fn uninstall_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("uninstall-target");
    let skill = tmp.child("uninstallable");
    write_skill(&skill, "uninstallable", "use when uninstallable");
    set_local_target(cfg.path(), cache.path(), target.path());

    cmd(cfg.path(), cache.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let removed = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "uninstallable",
            "--target",
            "local",
            "--yes",
        ])
        .assert()
        .success();
    assert!(removed.get_output().stderr.is_empty());
    let json = parse(&removed.get_output().stdout);

    assert_required_top_level_keys(
        &json,
        &["removed", "source_type", "source_ref", "version", "hash"],
    );
    assert_eq!(json["removed"]["skill"].as_str(), Some("uninstallable"));
    assert_eq!(json["removed"]["target"].as_str(), Some("local"));
    assert!(json["removed"]["path"].is_string());
    assert_eq!(json["source_type"].as_str(), Some("local"));
    assert!(json["source_ref"].is_string());
    assert!(json["version"].is_string() || json["version"].is_null());
    assert!(json["hash"].is_string() || json["hash"].is_null());
}

#[test]
fn uninstall_dry_run_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("dry-run-target");
    let skill = tmp.child("dry-run-jsonable");
    write_skill(&skill, "dry-run-jsonable", "use when dry running json");
    set_local_target(cfg.path(), cache.path(), target.path());

    cmd(cfg.path(), cache.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let dry_run = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "uninstall",
            "dry-run-jsonable",
            "--target",
            "local",
            "--dry-run",
        ])
        .assert()
        .success();
    assert!(dry_run.get_output().stderr.is_empty());
    let json = parse(&dry_run.get_output().stdout);

    assert_required_top_level_keys(
        &json,
        &[
            "would_remove",
            "source_type",
            "source_ref",
            "version",
            "hash",
            "dry_run",
        ],
    );
    assert_eq!(
        json["would_remove"]["skill"].as_str(),
        Some("dry-run-jsonable")
    );
    assert_eq!(json["would_remove"]["target"].as_str(), Some("local"));
    assert!(json["would_remove"]["path"].is_string());
    assert_eq!(json["dry_run"], Value::Bool(true));
}

#[test]
fn installed_json_contract_keys_and_types() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let target = tmp.child("installed-target");
    let skill = tmp.child("installed-jsonable");
    write_skill(
        &skill,
        "installed-jsonable",
        "use when listing installed json",
    );
    set_local_target(cfg.path(), cache.path(), target.path());

    cmd(cfg.path(), cache.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "local",
        ])
        .assert()
        .success();

    let list = cmd(cfg.path(), cache.path())
        .current_dir(tmp.path())
        .args(["--json", "install", "list"])
        .assert()
        .success();
    assert!(list.get_output().stderr.is_empty());
    let json = parse(&list.get_output().stdout);
    assert_required_top_level_keys(&json, &["installed"]);
    let installed = json["installed"].as_array().expect("installed array");
    assert_eq!(installed.len(), 1);
    for key in [
        "skill_name",
        "target",
        "source_type",
        "source_ref",
        "registry_url",
        "org",
        "version",
        "hash",
        "hash_kind",
        "installed_path",
        "installed_at",
        "installed_by",
        "receipt",
    ] {
        assert!(
            installed[0].get(key).is_some(),
            "missing installed row key `{key}` in {:?}",
            installed[0]
        );
    }
    assert_eq!(installed[0]["hash_kind"].as_str(), Some("install_tree"));

    let inspect = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "show",
            "installed-jsonable",
            "--target",
            "local",
        ])
        .assert()
        .success();
    assert!(inspect.get_output().stderr.is_empty());
    let json = parse(&inspect.get_output().stdout);
    assert_required_top_level_keys(
        &json,
        &["receipt", "receipt_path", "validation", "hash_kind"],
    );
    assert_eq!(
        json["receipt"]["skill_name"].as_str(),
        Some("installed-jsonable")
    );
    assert!(json["receipt_path"].is_string());
    assert_eq!(json["hash_kind"].as_str(), Some("install_tree"));
    assert_eq!(json["validation"]["ok"], Value::Bool(true));
    assert!(json["validation"]["errors"].is_array());
}

#[test]
fn skill_install_json_unknown_target_errors_only_json() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("ambiguous-installable");
    write_skill(&skill, "ambiguous-installable", "use when ambiguous");

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "--json",
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "vscode",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains("next:"));
    let json: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be JSON in --json mode, got `{stderr}`: {e}"));
    assert_eq!(json["error"]["code"].as_str(), Some("invalid_target"));
    assert_eq!(json["error"]["resource"].as_str(), Some("vscode"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack target list")
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown install target")
    );
}

#[test]
fn skill_install_human_unknown_target_prints_next_command() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let skill = tmp.child("ambiguous-installable");
    write_skill(&skill, "ambiguous-installable", "use when ambiguous");

    let assert = cmd(cfg.path(), cache.path())
        .args([
            "skill",
            "install",
            skill.path().to_str().unwrap(),
            "--target",
            "vscode",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("error: unknown install target `vscode`"));
    assert!(stderr.contains("next: agentstack target list"));
}

#[test]
fn clap_usage_errors_respect_json_flag() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let assert = cmd(cfg.path(), cache.path())
        .args(["--json", "skill", "install", "acme/review"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let json: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be JSON in --json mode, got `{stderr}`: {e}"));
    assert_eq!(json["error"]["code"].as_str(), Some("usage_error"));
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--target <TARGET>")
    );
}

#[test]
fn whoami_json_shape_when_logged_out() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    let token_file = tmp.child("tokens.json");

    let out = cmd(cfg.path(), cache.path())
        .env("AGENTSTACK_TOKEN_FILE", token_file.path())
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .args(["--json", "auth", "whoami"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let json = parse(&out);
    assert_eq!(json["error"]["code"].as_str(), Some("unauthenticated"));
    assert_eq!(json["error"]["status"].as_str(), Some("not_logged_in"));
    assert_eq!(
        json["error"]["machine_hint"].as_str(),
        Some("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
    );
    let methods = json["error"]["auth_methods"].as_array().unwrap();
    assert!(
        methods
            .iter()
            .any(|method| method.as_str() == Some("AGENTSTACK_TOKEN_PATH"))
    );
    assert!(
        methods
            .iter()
            .any(|method| method.as_str() == Some("AGENTSTACK_TOKEN"))
    );
}

#[test]
fn errors_in_json_mode_print_json_error_envelope_to_stderr() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let assert = cmd(cfg.path(), cache.path())
        .args(["--json", "skill", "list"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let json: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be JSON in --json mode, got `{stderr}`: {e}"));
    assert_eq!(json["error"]["code"].as_str(), Some("unauthenticated"));
    assert!(json["error"]["message"].is_string());
}

#[test]
fn list_local_json_does_not_emit_freeform_warnings() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");
    write_skill(&tmp.child("alpha"), "alpha", "use when alpha");
    let broken = tmp.child("broken");
    broken.create_dir_all().unwrap();
    broken
        .child("SKILL.md")
        .write_str("# missing frontmatter")
        .unwrap();

    let assert = cmd(cfg.path(), cache.path())
        .current_dir(tmp.path())
        .args(["--json", "skill", "scan"])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());
    let json = parse(&assert.get_output().stdout);
    let skills = json["skills"].as_array().expect("skills array");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"].as_str(), Some("alpha"));
}

#[test]
fn cache_remove_json_without_force_uses_error_envelope_only() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let cache = tmp.child("cache");

    let assert = cmd(cfg.path(), cache.path())
        .args(["--json", "cache", "remove", "ghost"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let json: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be JSON in --json mode, got `{stderr}`: {e}"));
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no cached skill named")
    );
    assert!(
        !json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--force")
    );
}
