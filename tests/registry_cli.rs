//! CLI-surface tests for registry push, export, search, list, versions, and
//! installs. These tests assert that:
//!
//! - The CLI parses correctly and routes to the right handler.
//! - Pre-network validation runs before the registry call (so bad refs and
//!   bad orgs fail with helpful messages, not transport errors).
//! - When a real HTTP call fails, the error mentions the active URL but
//!   never the user's token.
//! - The `--json` flag is accepted everywhere it's documented.
//!
//! Workflow correctness is tested in `tests/registry_workflow.rs` against a
//! [`MockRegistryClient`].
//!
//! [`MockRegistryClient`]: agentstack::registry::MockRegistryClient

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use agentstack::credentials::scoped_account;
use agentstack::package::{PackageHash, build_skill_package};
use agentstack::receipt::{RECEIPT_SCHEMA_VERSION, StackInstallReceipt, write_stack_receipt};
use agentstack::registry::Visibility;

fn write_skill(dir: &ChildPath, name: &str, description: &str) {
    dir.create_dir_all().unwrap();
    let body = format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
    );
    dir.child("SKILL.md").write_str(&body).unwrap();
}

fn fresh_env() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let token_file = tmp.child("tokens.json");
    (
        tmp,
        cfg.path().to_path_buf(),
        token_file.path().to_path_buf(),
    )
}

fn cmd(cfg: &std::path::Path, token_file: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("agentstack").unwrap();
    c.env("AGENTSTACK_CONFIG_DIR", cfg)
        .env("AGENTSTACK_TOKEN_FILE", token_file)
        .env("AGENTSTACK_ALLOW_TOKEN_FILE", "1")
        .env_remove("AGENTSTACK_TOKEN")
        .env_remove("AGENTSTACK_REGISTRY_URL");
    c
}

fn login(cfg: &std::path::Path, token_file: &std::path::Path, url: &str, token: &str) {
    cmd(cfg, token_file)
        .args(["registry", "use", url])
        .assert()
        .success();

    let account = scoped_account(url, "default").unwrap();
    std::fs::write(
        token_file,
        serde_json::json!({ account: token }).to_string(),
    )
    .unwrap();
}

fn set_registry(cfg: &std::path::Path, token_file: &std::path::Path, url: &str) {
    cmd(cfg, token_file)
        .args(["registry", "use", url])
        .assert()
        .success();
}

fn unused_registry_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    format!("http://{addr}")
}

struct HttpResponse {
    status_line: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

fn json_response(body: Value) -> HttpResponse {
    HttpResponse {
        status_line: "200 OK",
        content_type: "application/json",
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn json_status_response(status_line: &'static str, body: Value) -> HttpResponse {
    HttpResponse {
        status_line,
        content_type: "application/json",
        body: serde_json::to_vec(&body).unwrap(),
    }
}

fn text_status_response(status_line: &'static str, body: &str) -> HttpResponse {
    HttpResponse {
        status_line,
        content_type: "text/plain",
        body: body.as_bytes().to_vec(),
    }
}

fn bytes_response(content_type: &'static str, body: Vec<u8>) -> HttpResponse {
    HttpResponse {
        status_line: "200 OK",
        content_type,
        body,
    }
}

fn registry_server(responses: Vec<HttpResponse>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(read_http_request(&mut stream));
            let header = format!(
                "HTTP/1.1 {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response.status_line,
                response.content_type,
                response.body.len(),
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&response.body).unwrap();
        }
        requests
    });
    (url, handle)
}

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 8192];
    loop {
        let n = stream.read(&mut tmp).unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(header_end) = find_header_end(&buf) {
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let body_len = content_length(&headers);
            if buf.len() >= header_end + body_len {
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn content_length(headers: &str) -> usize {
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn remote_metadata(name: &str, version: &str, hash: &PackageHash) -> Value {
    serde_json::json!({
        "name": name,
        "description": format!("Use when {name} tasks come up"),
        "org": "acme",
        "visibility": "org",
        "version": version,
        "hash": hash,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    })
}

fn push_response(metadata: Value, url: Option<&str>) -> Value {
    let org = metadata["org"].as_str().unwrap().to_string();
    let name = metadata["name"].as_str().unwrap().to_string();
    let version = metadata["version"].as_str().unwrap().to_string();
    let sha256 = metadata["hash"]["hex"].as_str().unwrap().to_string();
    let visibility = metadata["visibility"].as_str().unwrap().to_string();
    let mut response = serde_json::json!({
        "metadata": metadata,
        "skill_ref": format!("{org}/{name}@{version}"),
        "version": version,
        "sha256": sha256,
        "visibility": visibility,
    });
    if let Some(url) = url {
        response["url"] = Value::String(url.to_string());
    }
    response
}

fn whoami_json() -> Value {
    serde_json::json!({
        "user": "pilot@example.com",
        "org": "acme",
        "email": "pilot@example.com",
        "name": "Pilot User",
        "server_admin": false,
        "orgs": [
            { "slug": "acme", "name": "Acme", "role": "reader" }
        ]
    })
}

fn whoami_two_orgs_json() -> Value {
    serde_json::json!({
        "user": "pilot@example.com",
        "org": "acme",
        "email": "pilot@example.com",
        "name": "Pilot User",
        "server_admin": false,
        "orgs": [
            { "slug": "acme", "name": "Acme", "role": "reader" },
            { "slug": "beta", "name": "Beta", "role": "reader" }
        ]
    })
}

#[test]
fn push_without_token_errors_before_registry_call_with_default_registry() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("agentstack auth login"));
}

#[test]
fn push_without_token_errors_before_registry_call() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");
    set_registry(&cfg, &token_file, "https://registry.example.com");

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("registry request failed").not());
}

#[test]
fn json_missing_auth_error_has_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    set_registry(&cfg, &token_file, "https://registry.example.com");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "list"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("unauthenticated"));
    assert_eq!(json["error"]["action"].as_str(), Some("authenticate"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack auth login")
    );
    assert_eq!(
        json["error"]["machine_hint"].as_str(),
        Some("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
    );
    let methods = json["error"]["auth_methods"].as_array().unwrap();
    assert!(
        methods
            .iter()
            .any(|method| method.as_str() == Some("auth_login"))
    );
    assert!(
        methods
            .iter()
            .any(|method| method.as_str() == Some("AGENTSTACK_TOKEN_PATH"))
    );
}

#[test]
fn json_invalid_skill_ref_error_includes_template_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "version", "list", "Bad_Name"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("invalid_skill_ref"));
    assert_eq!(json["error"]["resource"].as_str(), Some("Bad_Name"));
    assert_eq!(json["error"]["action"].as_str(), Some("parse_skill_ref"));
    assert!(json["error"].get("next_command").is_none());
    assert_eq!(
        json["error"]["next_command_template"].as_str(),
        Some("agentstack skill search <query>")
    );
}

#[test]
fn remote_skill_commands_validate_refs_before_auth_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    for args in [
        vec!["skill", "show", "Bad_Name"],
        vec!["skill", "status", "Bad_Name"],
        vec!["skill", "impact", "Bad_Name"],
        vec!["skill", "version", "show", "Bad_Name@1"],
        vec!["skill", "visibility", "show", "Bad_Name"],
        vec!["skill", "audit", "Bad_Name"],
        vec!["skill", "diff", "Bad_Name", "code-review@1"],
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("invalid skill name"))
            .stderr(predicate::str::contains("not logged in").not());
    }
}

#[test]
fn remote_skill_commands_validate_local_option_conflicts_before_auth_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/code-review", "--team", "engineering"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));

    cmd(&cfg, &token_file)
        .args(["skill", "version", "show", "code-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "skill version show expects `skill@version` or `org/skill@version`",
        ))
        .stderr(predicate::str::contains("not logged in").not());

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "diff",
            "code-review",
            "code-review@2",
            "--allow-yanked",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-yanked requires explicit pinned refs",
        ))
        .stderr(predicate::str::contains("not logged in").not());

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "export",
            "code-review",
            "--allow-yanked",
            "--out",
            ".",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-yanked requires an explicit pinned ref",
        ))
        .stderr(predicate::str::contains("not logged in").not());

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "install",
            "code-review",
            "--allow-yanked",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-yanked requires an explicit pinned ref",
        ))
        .stderr(predicate::str::contains("not logged in").not());

    for args in [
        vec![
            "skill",
            "version",
            "list",
            "acme/code-review",
            "--team",
            "engineering",
        ],
        vec![
            "skill",
            "version",
            "approve",
            "acme/code-review@1",
            "--team",
            "engineering",
        ],
        vec![
            "skill",
            "version",
            "yank",
            "acme/code-review@1",
            "--team",
            "engineering",
            "--reason",
            "bad",
        ],
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("not logged in"));
    }
}

#[test]
fn json_http_error_has_status_resource_and_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_status_response(
        "404 Not Found",
        serde_json::json!({
            "error": {
                "code": "skill_not_found",
                "message": "no such skill `acme/missing`",
                "http_status": 404
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/missing"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("skill_not_found"));
    assert_eq!(json["error"]["resource"].as_str(), Some("acme/missing"));
    assert_eq!(json["error"]["http_status"].as_u64(), Some(404));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack skill search missing --org acme")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/missing "),
        "{}",
        requests[0]
    );
}

#[test]
fn verbose_json_registry_error_stderr_is_single_parseable_envelope() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_status_response(
        "404 Not Found",
        serde_json::json!({
            "error": {
                "code": "skill_not_found",
                "message": "no such skill `acme/missing`",
                "http_status": 404
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--verbose", "--json", "skill", "show", "acme/missing"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains("[verbose]"), "stderr: {stderr}");
    let json: Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be one JSON object, got `{stderr}`: {e}"));
    assert_eq!(json["error"]["code"].as_str(), Some("skill_not_found"));
    assert_eq!(json["error"]["http_status"].as_u64(), Some(404));
    assert!(
        json["error"]["causes"]
            .as_array()
            .expect("causes array")
            .is_empty()
    );

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
}

#[test]
fn json_semantic_registry_error_omits_ping_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_status_response(
        "409 Conflict",
        serde_json::json!({
            "error": {
                "code": "already_yanked",
                "message": "version is already yanked",
                "http_status": 409
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/code-review@1"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("already_yanked"));
    assert_eq!(
        json["error"]["resource"].as_str(),
        Some("acme/code-review@1")
    );
    assert_eq!(json["error"]["http_status"].as_u64(), Some(409));
    assert!(json["error"].get("next_command").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions/1 "),
        "{}",
        requests[0]
    );
}

#[test]
fn registry_errors_redact_server_body_in_human_and_json_output() {
    let (_tmp, cfg, token_file) = fresh_env();
    let secret = "secrettokenvalue1234";
    let body = serde_json::json!({
        "error": {
            "code": "skill_not_found",
            "message": format!("missing; Authorization: Bearer {secret}; token={secret}")
        }
    });
    let (url, handle) = registry_server(vec![
        json_status_response("404 Not Found", body.clone()),
        json_status_response("404 Not Found", body),
    ]);
    login(&cfg, &token_file, &url, secret);

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("skill_not_found"));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn skill_impact_gets_visible_stack_usage() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skill": {
            "org": "acme",
            "name": "code-review",
            "latest_version": "2",
            "current_version": "1",
            "description": "Use when reviewing code",
            "visibility": "org"
        },
        "summary": {
            "used_by_count": 1,
            "current_policy_count": 0,
            "pinned_count": 1,
            "visible_only": true
        },
        "used_by": [
            {
                "stack": "acme/engineering-default",
                "org": "acme",
                "slug": "engineering-default",
                "name": "Engineering Default",
                "visibility": "org",
                "version_policy": "pinned",
                "pinned_version": "1",
                "effective_version": "1",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "impact", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("skill: acme/code-review"))
        .stdout(predicate::str::contains("impacted stacks: 1"))
        .stdout(predicate::str::contains("acme/engineering-default pins v1"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("Next:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/impact "),
        "{}",
        requests[0]
    );
}

#[test]
fn registry_errors_redact_bare_active_token_in_json_error_message() {
    let (_tmp, cfg, token_file) = fresh_env();
    let secret = "bareactivetokenjson1234";
    let body = serde_json::json!({
        "error": {
            "code": "skill_not_found",
            "message": secret
        }
    });
    let (url, handle) = registry_server(vec![
        json_status_response("404 Not Found", body.clone()),
        json_status_response("404 Not Found", body),
    ]);
    login(&cfg, &token_file, &url, secret);

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("skill_not_found"));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn registry_errors_redact_bare_active_token_in_text_error_body() {
    let (_tmp, cfg, token_file) = fresh_env();
    let secret = "bareactivetokentext1234";
    let (url, handle) = registry_server(vec![
        text_status_response("500 Internal Server Error", secret),
        text_status_response("500 Internal Server Error", secret),
    ]);
    login(&cfg, &token_file, &url, secret);

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains("[REDACTED]"));
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("registry_http_error"));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn registry_errors_do_not_trust_unknown_server_error_codes() {
    let (_tmp, cfg, token_file) = fresh_env();
    let secret = "servercodesecret1234";
    let (url, handle) = registry_server(vec![json_status_response(
        "500 Internal Server Error",
        serde_json::json!({
            "error": {
                "code": format!("token={secret}"),
                "message": "backend failed"
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/missing"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(secret).not());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("registry_error"));
    assert_eq!(json["error"]["http_status"].as_u64(), Some(500));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
}

#[test]
fn team_not_found_json_error_points_to_team_list() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_status_response(
        "404 Not Found",
        serde_json::json!({
            "error": {
                "code": "team_not_found",
                "message": "team not found"
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "team", "inspect", "acme/platform"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["resource"].as_str(), Some("acme/platform"));
    assert_eq!(json["error"]["action"].as_str(), Some("team"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack team list --org acme")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/teams/platform "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_sends_bearer_auth_header() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "missing"])
        .assert()
        .success();

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer secrettokenvalue1234"),
        "{}",
        requests[0]
    );
}

#[test]
fn hosted_auth_errors_have_actionable_hints_without_token_leaks() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_status_response(
        "401 Unauthorized",
        serde_json::json!({
            "error": {
                "code": "unauthenticated",
                "message": "missing or invalid token"
            }
        }),
    )]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "code-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("401 Unauthorized"))
        .stderr(predicate::str::contains("not authenticated"))
        .stderr(predicate::str::contains("agentstack auth login"))
        .stderr(predicate::str::contains("secrettokenvalue1234").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer secrettokenvalue1234"),
        "{}",
        requests[0]
    );
}

#[test]
fn push_rejects_bad_org_before_calling_registry() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "Bad_Org"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --org"));
}

#[test]
fn json_invalid_org_error_has_resource() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "Bad_Org"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("invalid_org"));
    assert_eq!(json["error"]["resource"].as_str(), Some("Bad_Org"));
    assert_eq!(json["error"]["action"].as_str(), Some("validate_org"));
}

#[test]
fn push_rejects_unknown_visibility() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "public"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid value"));
}

#[test]
fn push_rejects_missing_path_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["skill", "push", "/definitely/missing/path", "--org", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid skill"))
        .stderr(predicate::str::contains("not_a_directory"))
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn push_rejects_team_visibility_without_team() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "team"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--team is required"))
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn push_rejects_team_flag_without_team_visibility() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--team", "platform"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--team can only be used with --scope team",
        ));
}

#[test]
fn push_against_unreachable_registry_surfaces_clean_error() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("alpha");
    write_skill(&skill, "alpha", "Use when alpha tasks come up");
    let url = unused_registry_url();

    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let output = cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "private", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(url.as_str()))
        .stderr(predicate::str::contains("secrettokenvalue1234").not())
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("registry request failed") || stderr.contains("push to "),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn push_quiet_suppresses_success_output() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("quiet-push");
    write_skill(&skill, "quiet-push", "Use when quiet push tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("quiet-push", "local-dev", &built.hash);
    let response = push_response(
        metadata,
        Some("https://registry.example.com/acme/quiet-push"),
    );
    let (url, handle) = registry_server(vec![json_response(response)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "org", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn push_omits_org_when_active_token_has_one_org() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("inferred-push");
    write_skill(
        &skill,
        "inferred-push",
        "Use when inferred push tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("inferred-push", "local-dev", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(push_response(metadata, None)),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "push"])
        .arg(skill.path())
        .args(["--scope", "org", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("POST /v1/orgs/acme/skills "),
        "{}",
        requests[1]
    );
}

#[test]
fn push_json_success_includes_contract_fields() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-push");
    write_skill(&skill, "json-push", "Use when json push tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("json-push", "local-dev", &built.hash);
    let (url, handle) = registry_server(vec![json_response(push_response(metadata, None))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "org", "--yes"])
        .assert()
        .success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains("secrettokenvalue1234"));
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/json-push@local-dev"));
    assert_eq!(json["version"].as_str(), Some("local-dev"));
    assert_eq!(json["sha256"].as_str(), Some(built.hash.hex.as_str()));
    assert_eq!(json["visibility"].as_str(), Some("org"));
    assert!(json.get("would_upload").is_none());
    assert!(json["url"].is_null());
    assert_eq!(json["metadata"]["name"].as_str(), Some("json-push"));
    assert_eq!(
        json["metadata"]["hash"]["hex"].as_str(),
        Some(built.hash.hex.as_str())
    );
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill version approve acme/json-push@local-dev")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack skill version list acme/json-push")
    );
    assert!(json.get("next_command_templates").is_none());
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn push_json_current_version_includes_status_next_command() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-current-push");
    write_skill(
        &skill,
        "json-current-push",
        "Use when json current push tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let mut metadata = remote_metadata("json-current-push", "local-dev", &built.hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    let (url, handle) = registry_server(vec![json_response(push_response(metadata, None))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "org", "--yes"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["skill_ref"].as_str(),
        Some("acme/json-current-push@local-dev")
    );
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill status acme/json-current-push")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack skill version list acme/json-current-push")
    );
    assert!(json.get("next_command_templates").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn push_json_without_yes_refuses_to_upload() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-push-needs-yes");
    write_skill(
        &skill,
        "json-push-needs-yes",
        "Use when checking json push confirmation safety",
    );
    let (url, handle) = registry_server(vec![]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "org"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rerun with `--yes`"));

    let requests = handle.join().unwrap();
    assert!(requests.is_empty(), "{requests:?}");
}

#[test]
fn dry_run_human_output_describes_local_plan() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("dry-run-push");
    write_skill(
        &skill,
        "dry-run-push",
        "Use when dry-run push tasks come up",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--dry-run", "--platform", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "local push plan for acme/dry-run-push@local-dev",
        ))
        .stdout(predicate::str::contains(
            "authorization: not checked (dry run does not contact the registry)",
        ))
        .stdout(predicate::str::contains("not uploaded"))
        .stdout(predicate::str::contains("platforms:  codex"))
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn single_skill_push_accepts_yes_without_all() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("single-yes-push");
    write_skill(
        &skill,
        "single-yes-push",
        "Use when checking single skill push confirmation flags",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--dry-run", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "local push plan for acme/single-yes-push@local-dev",
        ));
}

#[test]
fn push_private_visibility_human_output_warns_about_default_privacy() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("private-push");
    write_skill(
        &skill,
        "private-push",
        "Use when private push tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let mut metadata = remote_metadata("private-push", "local-dev", &built.hash);
    // The default --visibility is private; the server is expected to echo
    // it back. The mock fixture must mirror that or render_human will see
    // the wrong value on the response path.
    metadata["visibility"] = Value::String("private".to_string());
    let (url, _handle) = registry_server(vec![json_response(push_response(metadata, None))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        // No --visibility => the default `private`. The user must see that
        // the push is not visible to the rest of the org.
        .args(["--org", "acme", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private — only you and admins"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("Next steps").not())
        .stdout(predicate::str::contains("--scope org"))
        .stdout(predicate::str::contains("Use --scope org").not())
        .stdout(predicate::str::contains(
            "candidate: approve before readers install it.",
        ))
        .stdout(predicate::str::contains("Uploaded as a candidate").not())
        .stdout(predicate::str::contains(
            "agentstack skill version approve acme/private-push@local-dev",
        ))
        .stdout(predicate::str::contains(
            "agentstack skill version list acme/private-push",
        ));
}

#[test]
fn push_private_visibility_dry_run_warns_about_default_privacy() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("private-dry-run-push");
    write_skill(
        &skill,
        "private-dry-run-push",
        "Use when private dry-run push tasks come up",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("private — only you and admins"))
        .stdout(predicate::str::contains("--scope org"))
        .stdout(predicate::str::contains("Use --scope org").not());
}

#[test]
fn dry_run_json_output_shape() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-dry-run");
    write_skill(
        &skill,
        "json-dry-run",
        "Use when json dry-run tasks come up",
    );

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--scope", "org", "--dry-run"])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["would_upload"].as_bool(), Some(true));
    assert_eq!(json["authorization_checked"].as_bool(), Some(false));
    assert_eq!(
        json["skill_ref"].as_str(),
        Some("acme/json-dry-run@local-dev")
    );
    assert_eq!(json["version"].as_str(), Some("local-dev"));
    assert_eq!(json["visibility"].as_str(), Some("org"));
    assert_eq!(json["metadata"]["name"].as_str(), Some("json-dry-run"));
    assert_eq!(json["sha256"].as_str().unwrap().len(), 64);
    assert!(json["size_bytes"].as_u64().unwrap() > 0);
}

#[test]
fn dry_run_omitted_org_reports_authorization_checked() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("inferred-dry-run");
    write_skill(
        &skill,
        "inferred-dry-run",
        "Use when inferred dry-run tasks come up",
    );
    let (url, handle) = registry_server(vec![json_response(whoami_json())]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--scope", "org", "--dry-run"])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["skill_ref"].as_str(),
        Some("acme/inferred-dry-run@local-dev")
    );
    assert_eq!(json["authorization_checked"].as_bool(), Some(true));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
}

#[test]
fn omitted_org_multi_org_command_hint_uses_org_flag() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("multi-org-push");
    write_skill(
        &skill,
        "multi-org-push",
        "Use when multi org push tasks come up",
    );
    let (url, handle) = registry_server(vec![json_response(whoami_two_orgs_json())]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--scope", "org", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass `--org <org>`"))
        .stderr(predicate::str::contains("org/skill push").not());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
}

#[test]
fn push_team_visibility_includes_team_in_metadata() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("team-dry-run");
    write_skill(
        &skill,
        "team-dry-run",
        "Use when team dry-run tasks come up",
    );

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args([
            "--org",
            "acme",
            "--scope",
            "team",
            "--team",
            "engineering",
            "--dry-run",
        ])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["visibility"].as_str(), Some("team"));
    assert_eq!(json["metadata"]["visibility"].as_str(), Some("team"));
    assert_eq!(json["metadata"]["team"].as_str(), Some("engineering"));
}

#[test]
fn dry_run_json_includes_lint_warnings() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-dry-run-lint");
    skill.create_dir_all().unwrap();
    skill
        .child("SKILL.md")
        .write_str(
            "---\nname: json-dry-run-lint\ndescription: helpful skill\n---\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(skill.path())
        .args(["--org", "acme", "--dry-run"])
        .assert()
        .success();

    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    let warnings = json["lint_warnings"].as_array().unwrap();
    assert!(!warnings.is_empty());
}

#[test]
fn push_json_invalid_skill_uses_error_envelope_only() {
    let (tmp, cfg, token_file) = fresh_env();
    let broken = tmp.child("broken");
    broken.create_dir_all().unwrap();

    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "push"])
        .arg(broken.path())
        .args(["--org", "acme"])
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
            .contains("not a valid skill")
    );
}

#[test]
fn teams_commands_send_expected_requests_and_redact_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let team_empty = serde_json::json!({
        "team": { "org": "acme", "slug": "platform", "members": [] }
    });
    let team_with_admin = serde_json::json!({
        "team": {
            "org": "acme",
            "slug": "platform",
            "members": [
                { "email": "lead@example.com", "role": "team_admin" }
            ]
        }
    });
    let (url, handle) = registry_server(vec![
        json_response(serde_json::json!({
            "team": team_empty["team"],
            "audit_event_id": "aud_team_created"
        })),
        json_response(serde_json::json!({
            "teams": [{ "org": "acme", "slug": "platform" }]
        })),
        json_response(team_with_admin.clone()),
        json_response(serde_json::json!({
            "team": team_with_admin["team"],
            "audit_event_id": "aud_member_added"
        })),
        json_response(serde_json::json!({
            "team": team_with_admin["team"],
            "audit_event_id": "aud_member_role_changed"
        })),
        json_response(serde_json::json!({
            "team": team_empty["team"],
            "audit_event_id": "aud_member_removed"
        })),
    ]);
    let secret = "teamsecretvalue1234";
    login(&cfg, &token_file, &url, secret);

    let create = cmd(&cfg, &token_file)
        .args(["--json", "team", "create", "acme/platform"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&create).unwrap();
    assert_eq!(json["team"]["org"].as_str(), Some("acme"));
    assert_eq!(json["team"]["slug"].as_str(), Some("platform"));
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_team_created"));
    assert!(json["team"].get("audit_event_id").is_none());

    let list = cmd(&cfg, &token_file)
        .args(["--json", "team", "list", "--org", "acme"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&list).unwrap();
    assert_eq!(json["teams"].as_array().unwrap().len(), 1);
    assert!(json.get("audit_event_id").is_none());

    let inspect = cmd(&cfg, &token_file)
        .args(["--json", "team", "inspect", "acme/platform"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&inspect).unwrap();
    assert_eq!(json["team"]["members"][0]["email"], "lead@example.com");
    assert!(json.get("audit_event_id").is_none());

    let add = cmd(&cfg, &token_file)
        .args([
            "--json",
            "team",
            "add-member",
            "acme/platform",
            "lead@example.com",
            "--role",
            "team_admin",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let set_role = cmd(&cfg, &token_file)
        .args([
            "--json",
            "team",
            "set-role",
            "acme/platform",
            "lead@example.com",
            "--role",
            "team_admin",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let remove = cmd(&cfg, &token_file)
        .args([
            "--json",
            "team",
            "remove-member",
            "acme/platform",
            "lead@example.com",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&add).unwrap();
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_member_added"));
    assert!(json["team"].get("audit_event_id").is_none());
    let json: Value = serde_json::from_slice(&set_role).unwrap();
    assert_eq!(
        json["audit_event_id"].as_str(),
        Some("aud_member_role_changed")
    );
    assert!(json["team"].get("audit_event_id").is_none());
    let json: Value = serde_json::from_slice(&remove).unwrap();
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_member_removed"));
    assert!(json["team"].get("audit_event_id").is_none());

    for bytes in [create, list, inspect, add, set_role, remove] {
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(secret), "team JSON leaked token: {text}");
    }

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/teams "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains(r#""slug":"platform""#),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/teams "),
        "{}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("GET /v1/orgs/acme/teams/platform "),
        "{}",
        requests[2]
    );
    assert!(
        requests[3].starts_with("PUT /v1/orgs/acme/teams/platform/members/lead%40example.com "),
        "{}",
        requests[3]
    );
    assert!(
        requests[3].contains(r#""role":"team_admin""#),
        "{}",
        requests[3]
    );
    assert!(
        requests[4].starts_with("PATCH /v1/orgs/acme/teams/platform/members/lead%40example.com "),
        "{}",
        requests[4]
    );
    assert!(
        requests[5].starts_with("DELETE /v1/orgs/acme/teams/platform/members/lead%40example.com "),
        "{}",
        requests[5]
    );
}

#[test]
fn team_mutation_human_output_prints_audit_event_id() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "team": { "org": "acme", "slug": "platform", "members": [] },
        "audit_event_id": "aud_team_created"
    }))]);
    login(&cfg, &token_file, &url, "teamsecretvalue1234");

    cmd(&cfg, &token_file)
        .args(["team", "create", "acme/platform"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created acme/platform"))
        .stdout(predicate::str::contains("audit_event_id: aud_team_created"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/teams "),
        "{}",
        requests[0]
    );
}

#[test]
fn team_list_empty_result_prints_empty_state() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "teams": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["team", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no teams found in `acme`."))
        .stdout(predicate::str::contains(
            "next: agentstack team create acme/<team>",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/teams "),
        "{}",
        requests[0]
    );
}

#[test]
fn team_list_json_empty_result_includes_empty_state_with_template_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "teams": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "team", "list", "--org", "acme"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert!(json["teams"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no teams found in `acme`.")
    );
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack team create acme/<team>")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/teams "),
        "{}",
        requests[0]
    );
}

#[test]
fn team_list_omits_org_when_active_token_has_one_org() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "teams": []
        })),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["team", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no teams found in `acme`."));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/teams "),
        "{}",
        requests[1]
    );
}

#[test]
fn stacks_commands_send_expected_requests_and_redact_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let stack_empty = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "description": "",
            "visibility": "team",
            "team": "engineering",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "items": []
        }
    });
    let stack_with_item = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "description": "",
            "visibility": "team",
            "team": "engineering",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "items": [{
                "skill": "incident-runbook",
                "version_policy": "current",
                "position": 0,
                "added_at": "2026-01-01T00:00:00Z"
            }]
        }
    });
    let stack_team_visible = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "description": "",
            "visibility": "team",
            "team": "engineering",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "items": []
        },
        "audit_event_id": "aud_stack_visibility"
    });
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "team",
            "team": "engineering"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": PackageHash::sha256_of(b"stack"),
        "items": []
    });
    let (url, handle) = registry_server(vec![
        json_response(stack_empty.clone()),
        json_response(serde_json::json!({
            "stacks": [{
                "org": "acme",
                "slug": "engineering-default",
                "name": "Engineering Default",
                "description": "",
                "visibility": "team",
                "team": "engineering",
                "item_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }]
        })),
        json_response(stack_with_item.clone()),
        json_response(stack_with_item.clone()),
        json_response(resolve),
        json_response(stack_empty),
        json_response(stack_team_visible.clone()),
        json_response(stack_team_visible),
    ]);
    let secret = "stacksecretvalue1234";
    login(&cfg, &token_file, &url, secret);

    let create = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "create",
            "acme/engineering-default",
            "--scope",
            "team",
            "--team",
            "engineering",
            "--name",
            "Engineering Default",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "list",
            "--org",
            "acme",
            "--owner",
            "owner@example.com",
            "--team",
            "engineering",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let add = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "add",
            "acme/engineering-default",
            "incident-runbook",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let inspect = cmd(&cfg, &token_file)
        .args(["--json", "stack", "show", "acme/engineering-default"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let resolve_stdout = cmd(&cfg, &token_file)
        .args(["--json", "stack", "resolve", "acme/engineering-default"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let remove = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "remove",
            "acme/engineering-default",
            "incident-runbook",
            "--yes",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let visibility = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "visibility",
            "set",
            "acme/engineering-default",
            "--scope",
            "team",
            "--team",
            "engineering",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let visibility_show = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "visibility",
            "show",
            "acme/engineering-default",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    for bytes in [
        create,
        list,
        add,
        inspect,
        resolve_stdout,
        remove,
        visibility,
        visibility_show,
    ] {
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(secret), "stack JSON leaked token: {text}");
    }

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/stacks "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains(r#""slug":"engineering-default""#),
        "{}",
        requests[0]
    );
    assert!(
        requests[1]
            .starts_with("GET /v1/orgs/acme/stacks?owner=owner%40example.com&team=engineering "),
        "{}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("POST /v1/orgs/acme/stacks/engineering-default/items "),
        "{}",
        requests[2]
    );
    assert!(
        requests[2].contains(r#""skill":"incident-runbook""#),
        "{}",
        requests[2]
    );
    assert!(
        requests[3].starts_with("GET /v1/orgs/acme/stacks/engineering-default "),
        "{}",
        requests[3]
    );
    assert!(
        requests[4].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[4]
    );
    assert!(
        requests[5]
            .starts_with("DELETE /v1/orgs/acme/stacks/engineering-default/items/incident-runbook "),
        "{}",
        requests[5]
    );
    assert!(
        requests[6].starts_with("PATCH /v1/orgs/acme/stacks/engineering-default/visibility "),
        "{}",
        requests[6]
    );
    assert!(
        requests[6].contains(r#""visibility":"team""#),
        "{}",
        requests[6]
    );
    assert!(
        requests[6].contains(r#""team":"engineering""#),
        "{}",
        requests[6]
    );
    assert!(
        requests[7].starts_with("GET /v1/orgs/acme/stacks/engineering-default "),
        "{}",
        requests[7]
    );
}

#[test]
fn stack_remove_dry_run_resolves_stack_without_mutation() {
    let (_tmp, cfg, token_file) = fresh_env();
    let stack_with_items = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "description": "",
            "visibility": "team",
            "team": "engineering",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "items": [
                {
                    "skill": "incident-runbook",
                    "version_policy": "current",
                    "position": 0,
                    "added_at": "2026-01-01T00:00:00Z"
                },
                {
                    "skill": "api-review-checklist",
                    "version_policy": "current",
                    "position": 1,
                    "added_at": "2026-01-01T00:00:00Z"
                }
            ]
        }
    });
    let (url, handle) = registry_server(vec![json_response(stack_with_items)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "remove",
            "acme/engineering-default",
            "incident-runbook",
            "--dry-run",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["dry_run"].as_bool(), Some(true));
    assert_eq!(json["would_remove"].as_str(), Some("incident-runbook"));
    assert_eq!(
        json["items_after"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["api-review-checklist"]
    );

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks/engineering-default "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_remove_noninteractive_without_yes_fails_before_registry_call() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "remove",
            "acme/engineering-default",
            "incident-runbook",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("rerun with `--yes`"));

    let requests = handle.join().unwrap();
    assert!(requests.is_empty(), "{requests:?}");
}

#[test]
fn stack_visibility_set_validates_team_args_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    for (args, expected) in [
        (
            vec![
                "stack",
                "visibility",
                "set",
                "acme/engineering-default",
                "--scope",
                "team",
            ],
            "--team is required",
        ),
        (
            vec![
                "stack",
                "visibility",
                "set",
                "acme/engineering-default",
                "--scope",
                "org",
                "--team",
                "engineering",
            ],
            "--team can only be used with --scope team",
        ),
        (
            vec![
                "stack",
                "visibility",
                "set",
                "acme/engineering-default",
                "--scope",
                "team",
                "--team",
                "Engineering",
            ],
            "invalid --team",
        ),
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected))
            .stderr(predicate::str::contains("no registry configured").not());
    }
}

#[test]
fn teams_validate_refs_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["team", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage teams and team membership"));

    cmd(&cfg, &token_file)
        .args(["team", "inspect", "missing-slash"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("team ref must be `org/team`"))
        .stderr(predicate::str::contains("no registry configured").not());

    cmd(&cfg, &token_file)
        .args(["team", "list", "--org", "Bad_Org"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --org"))
        .stderr(predicate::str::contains("no registry configured").not());

    for args in [
        [
            "team",
            "add-member",
            "acme/platform",
            "user@example.com",
            "--role",
            "lead",
        ],
        [
            "team",
            "set-role",
            "acme/platform",
            "user@example.com",
            "--role",
            "lead",
        ],
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "unknown role `lead` (expected one of: member, team_admin)",
            ))
            .stderr(predicate::str::contains("legacy lead").not())
            .stderr(predicate::str::contains("no registry configured").not());
    }
}

#[test]
fn stacks_validate_inputs_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["stack", "show", "acme/Bad_Stack"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));

    cmd(&cfg, &token_file)
        .args(["stack", "list", "--org", "Bad_Org"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid --org"))
        .stderr(predicate::str::contains("no registry configured").not());

    cmd(&cfg, &token_file)
        .args([
            "stack",
            "add",
            "acme/engineering-default",
            "incident-runbook",
            "--version-policy",
            "current",
            "--pin-version",
            "3",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn skill_export_rejects_bad_skill_ref_before_calling_registry() {
    let (_tmp, cfg, token_file) = fresh_env();
    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "export", "Bad_Name", "--out", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid skill name"));
}

#[test]
fn stack_status_json_includes_resolve_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "description": "",
            "visibility": "team",
            "team": "engineering",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "items": [{
                "skill": "incident-runbook",
                "version_policy": "current",
                "position": 0,
                "added_at": "2026-01-01T00:00:00Z"
            }]
        }
    }))]);
    login(&cfg, &token_file, &url, "stackstatussecret1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "stack", "status", "acme/engineering-default"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["stack"]["slug"].as_str(), Some("engineering-default"));
    assert_eq!(json["stack"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack stack resolve acme/engineering-default")
    );
    assert!(json.get("next_command_template").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks/engineering-default/status "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_export_without_org_uses_token_org_context() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["stack", "export", "engineering-default", "--out", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));
}

#[test]
fn stack_export_human_output_uses_next_block_for_managed_install() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("stack-child");
    write_skill(&skill, "stack-child", "Use when stack child tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("stack-child", "1", &built.hash);
    let manifest_hash = PackageHash::sha256_of(b"stack manifest");
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "org"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": manifest_hash,
        "items": [{
            "skill": "stack-child",
            "version_id": "ver_stack_child_1",
            "version": "1",
            "archive_hash": built.hash.clone(),
            "download": {
                "method": "GET",
                "url": "/v1/orgs/acme/skills/stack-child/versions/1/archive"
            },
            "version_policy": "current"
        }]
    });
    let (url, handle) = registry_server(vec![
        json_response(resolve),
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "stackexportsecret1234");
    let out = tmp.child("exported-stack");

    cmd(&cfg, &token_file)
        .args(["stack", "export", "acme/engineering-default", "--out"])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "exported stack acme/engineering-default",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack stack install acme/engineering-default --target <target>",
        ))
        .stdout(predicate::str::contains("Managed install").not());

    assert!(out.child("stack-child").child("SKILL.md").path().is_file());
    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/skills/stack-child/versions/1 "),
        "{}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("GET /v1/orgs/acme/skills/stack-child/versions/1/archive "),
        "{}",
        requests[2]
    );
}

#[test]
fn stack_export_json_includes_template_next_command() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("stack-json-child");
    write_skill(
        &skill,
        "stack-json-child",
        "Use when stack json export tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("stack-json-child", "1", &built.hash);
    let manifest_hash = PackageHash::sha256_of(b"stack manifest json");
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "org"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": manifest_hash,
        "items": [{
            "skill": "stack-json-child",
            "version_id": "ver_stack_json_child_1",
            "version": "1",
            "archive_hash": built.hash.clone(),
            "download": {
                "method": "GET",
                "url": "/v1/orgs/acme/skills/stack-json-child/versions/1/archive"
            },
            "version_policy": "current"
        }]
    });
    let (url, handle) = registry_server(vec![
        json_response(resolve),
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "stackexportjsonsecret1234");
    let out = tmp.child("exported-stack-json");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "export",
            "acme/engineering-default",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["stack"]["slug"].as_str(), Some("engineering-default"));
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
    assert!(json.get("next_commands").is_none());
    assert_eq!(
        json["next_command_templates"][0].as_str(),
        Some("agentstack stack install acme/engineering-default --target <target>")
    );
    assert!(
        !String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .contains("stackexportjsonsecret1234")
    );

    assert!(
        out.child("stack-json-child")
            .child("SKILL.md")
            .path()
            .is_file()
    );
    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_export_direct_ref_extra_positional_points_to_out_flag() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["skill", "export", "acme/code-review", "./skills"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"))
        .stderr(predicate::str::contains("unknown internal export source kind").not())
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn remote_install_without_token_errors_before_registry_call() {
    let (tmp, cfg, token_file) = fresh_env();
    let dest = tmp.child("target");
    set_registry(&cfg, &token_file, "https://registry.example.com");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["skill", "install", "acme/code-review", "--target", "local"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("registry request failed").not());
}

#[test]
fn remote_install_pinned_version_writes_receipt_without_token() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("remote-sql");
    write_skill(&skill, "remote-sql", "Use when remote sql tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("remote-sql", "7", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(metadata),
        bytes_response("application/gzip", built.bytes.clone()),
    ]);
    login(&cfg, &token_file, &url, "supersecrettokenvalue1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args(["skill", "install", "acme/remote-sql@7", "--target", "local"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "installed skill acme/remote-sql@7",
        ))
        .stdout(predicate::str::contains("source: acme/remote-sql"))
        .stdout(predicate::str::contains("package hash: sha256:"))
        .stdout(predicate::str::contains("receipt:"))
        .stdout(predicate::str::contains("supersecrettokenvalue1234").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/skills/remote-sql/versions/7 "),
        "{}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("GET /v1/orgs/acme/skills/remote-sql/versions/7/archive "),
        "{}",
        requests[2]
    );

    let receipt_path = dest.child("remote-sql").child(".agentstack-install.json");
    let receipt_text = std::fs::read_to_string(receipt_path.path()).unwrap();
    assert!(!receipt_text.contains("supersecrettokenvalue1234"));
    let receipt: Value = serde_json::from_str(&receipt_text).unwrap();
    assert_eq!(receipt["source_type"].as_str(), Some("registry"));
    assert_eq!(receipt["source_ref"].as_str(), Some("acme/remote-sql"));
    assert_eq!(receipt["version"].as_str(), Some("7"));
    let expected_hash = format!("sha256:{}", built.hash.hex);
    assert_eq!(receipt["hash"].as_str(), Some(expected_hash.as_str()));

    let inspect = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "remote-sql", "--target", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&inspect).unwrap();
    assert_eq!(json["receipt"]["version"].as_str(), Some("7"));
    assert_eq!(json["validation"]["ok"].as_bool(), Some(true));
}

#[test]
fn remote_install_json_stdout_is_json_only() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("remote-json");
    write_skill(&skill, "remote-json", "Use when remote json tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("remote-json", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(metadata),
        bytes_response("application/gzip", built.bytes.clone()),
    ]);
    login(&cfg, &token_file, &url, "jsoninstallsecret1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "--json",
            "skill",
            "install",
            "acme/remote-json@1",
            "--target",
            "local",
        ])
        .assert()
        .success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(!stderr.contains("jsoninstallsecret1234"));
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["source_type"].as_str(), Some("registry"));
    assert_eq!(json["version"].as_str(), Some("1"));
    assert_eq!(json["hash_kind"].as_str(), Some("package"));
    assert_eq!(json["kind"].as_str(), Some("skill_install"));
    assert_eq!(json["operation"].as_str(), Some("install"));
    assert_eq!(json["resource"].as_str(), Some("remote-json"));
    assert_eq!(
        json["next_commands"][2].as_str(),
        Some("agentstack skill update remote-json --target local --check")
    );
    assert_eq!(json["target"].as_str(), Some("local"));
    assert!(
        !String::from_utf8(assert.get_output().stdout.clone())
            .unwrap()
            .contains("jsoninstallsecret1234")
    );
    let _ = handle.join().unwrap();
}

#[test]
fn stack_install_json_no_input_installs_child_and_never_prints_token() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("stack-child");
    write_skill(&skill, "stack-child", "Use when stack child tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("stack-child", "1", &built.hash);
    let manifest_hash = PackageHash::sha256_of(b"stack manifest");
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "org"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": manifest_hash,
        "items": [{
            "skill": "stack-child",
            "version_id": "ver_stack_child_1",
            "version": "1",
            "archive_hash": built.hash.clone(),
            "download": {
                "method": "GET",
                "url": "/v1/orgs/acme/skills/stack-child/versions/1/archive"
            },
            "version_policy": "current"
        }]
    });
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(resolve),
        json_response(metadata),
        bytes_response("application/gzip", built.bytes.clone()),
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "stack": {
                "org": "acme",
                "slug": "engineering-default",
                "name": "Engineering Default",
                "visibility": "org"
            },
            "resolved_at": "2026-01-01T00:00:00Z",
            "manifest_hash": manifest_hash,
            "items": [{
                "skill": "stack-child",
                "version_id": "ver_stack_child_1",
                "version": "1",
                "archive_hash": built.hash.clone(),
                "download": {
                    "method": "GET",
                    "url": "/v1/orgs/acme/skills/stack-child/versions/1/archive"
                },
                "version_policy": "current"
            }]
        })),
        json_response(remote_metadata("stack-child", "1", &built.hash)),
        bytes_response("application/gzip", built.bytes.clone()),
    ]);
    login(&cfg, &token_file, &url, "stackinstallsecret1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "--json",
            "--no-input",
            "stack",
            "install",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("stackinstallsecret1234"));
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["stack"].as_str(), Some("engineering-default"));
    assert_eq!(json["kind"].as_str(), Some("stack_install"));
    assert_eq!(json["operation"].as_str(), Some("install"));
    assert_eq!(json["resource"].as_str(), Some("acme/engineering-default"));
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack stack show acme/engineering-default --target local")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack install doctor --target local")
    );
    assert!(dest.child("stack-child").child("SKILL.md").path().is_file());

    let child_receipt_text = std::fs::read_to_string(
        dest.child("stack-child")
            .child(".agentstack-install.json")
            .path(),
    )
    .unwrap();
    assert!(!child_receipt_text.contains("stackinstallsecret1234"));
    assert!(child_receipt_text.contains("\"installed_via\""));
    let child_receipt: Value = serde_json::from_str(&child_receipt_text).unwrap();
    assert!(
        child_receipt["hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        child_receipt["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    let stack_receipt = std::fs::read_to_string(
        dest.child(".agentstack-stacks")
            .child("acme")
            .child("engineering-default")
            .child(".agentstack.json")
            .path(),
    )
    .unwrap();
    assert!(!stack_receipt.contains("stackinstallsecret1234"));

    let list_assert = cmd(&cfg, &token_file)
        .args(["--json", "install", "list", "--kind", "stack"])
        .assert()
        .success();
    let list_json: Value = serde_json::from_slice(&list_assert.get_output().stdout).unwrap();
    assert_eq!(list_json["installed"].as_array().unwrap().len(), 1);
    assert_eq!(
        list_json["installed"][0]["stack"].as_str(),
        Some("engineering-default")
    );
    assert!(
        !String::from_utf8(list_assert.get_output().stdout.clone())
            .unwrap()
            .contains("stackinstallsecret1234")
    );

    let inspect_assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "show",
            "engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success();
    let inspect_json: Value = serde_json::from_slice(&inspect_assert.get_output().stdout).unwrap();
    assert_eq!(
        inspect_json["receipt"]["stack"].as_str(),
        Some("engineering-default")
    );
    assert_eq!(
        inspect_json["receipt"]["items"].as_array().unwrap().len(),
        1
    );
    assert!(
        !String::from_utf8(inspect_assert.get_output().stdout.clone())
            .unwrap()
            .contains("stackinstallsecret1234")
    );

    cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "stack",
            "install",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "refreshed existing stack acme/engineering-default",
        ))
        .stdout(predicate::str::contains("target: local"))
        .stdout(predicate::str::contains("source: acme/engineering-default"))
        .stdout(predicate::str::contains("receipt:"))
        .stdout(predicate::str::contains("(refreshed)"));

    let requests = handle.join().unwrap();
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[1]
    );
    assert!(
        requests[2].starts_with("GET /v1/orgs/acme/skills/stack-child/versions/1 "),
        "{}",
        requests[2]
    );
    assert!(
        requests[3].starts_with("GET /v1/orgs/acme/skills/stack-child/versions/1/archive "),
        "{}",
        requests[3]
    );
}

#[test]
fn stack_install_denied_does_not_register_target_or_create_dir() {
    let (tmp, cfg, token_file) = fresh_env();
    let home = tmp.child("home");
    let denied = serde_json::json!({
        "error": {
            "code": "forbidden",
            "message": "stack is not visible",
            "http_status": 403
        }
    });
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_status_response("403 Forbidden", denied),
    ]);
    login(&cfg, &token_file, &url, "deniedstacktoken1234");

    cmd(&cfg, &token_file)
        .env("HOME", home.path())
        .args([
            "stack",
            "install",
            "acme/private-stack",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to install stack"));

    assert!(!home.child(".agentstack").child("skills").path().exists());
    let config_path = cfg.join("config.toml");
    if config_path.exists() {
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(!config.contains("[targets.local]"), "config = {config}");
        assert!(
            !config.contains(
                home.child(".agentstack")
                    .child("skills")
                    .path()
                    .to_str()
                    .unwrap()
            ),
            "config = {config}"
        );
    }

    let requests = handle.join().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("GET /v1/orgs/acme/stacks/private-stack/resolve ")),
        "requests = {requests:?}"
    );
}

#[test]
fn skill_install_denied_does_not_register_target_or_create_dir() {
    let (tmp, cfg, token_file) = fresh_env();
    let home = tmp.child("home");
    let denied = serde_json::json!({
        "error": {
            "code": "forbidden",
            "message": "skill is not visible",
            "http_status": 403
        }
    });
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_status_response("403 Forbidden", denied),
    ]);
    login(&cfg, &token_file, &url, "deniedskilltoken1234");

    cmd(&cfg, &token_file)
        .env("HOME", home.path())
        .args([
            "skill",
            "install",
            "acme/private-skill",
            "--target",
            "local",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to install"));

    assert!(!home.child(".agentstack").child("skills").path().exists());
    let config_path = cfg.join("config.toml");
    if config_path.exists() {
        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(!config.contains("[targets.local]"), "config = {config}");
        assert!(
            !config.contains(
                home.child(".agentstack")
                    .child("skills")
                    .path()
                    .to_str()
                    .unwrap()
            ),
            "config = {config}"
        );
    }

    let requests = handle.join().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| { request.starts_with("GET /v1/orgs/acme/skills/private-skill ") }),
        "requests = {requests:?}"
    );
}

#[test]
fn stack_resolve_human_output_shows_scope_policy_and_hashes() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"resolved-child");
    let manifest_hash = PackageHash::sha256_of(b"resolved-stack");
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "team",
            "team": "engineering"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": manifest_hash,
        "items": [{
            "skill": "incident-runbook",
            "version_id": "ver_incident_1",
            "version": "1",
            "archive_hash": hash,
            "download": {
                "method": "GET",
                "url": "/v1/orgs/acme/skills/incident-runbook/versions/1/archive"
            },
            "version_policy": "current"
        }]
    });
    let (url, handle) = registry_server(vec![json_response(resolve)]);
    login(&cfg, &token_file, &url, "resolversecret1234");

    cmd(&cfg, &token_file)
        .args(["stack", "resolve", "acme/engineering-default"])
        .assert()
        .success()
        .stdout(predicate::str::contains("visibility: team"))
        .stdout(predicate::str::contains("team:       engineering"))
        .stdout(predicate::str::contains("resolved:   2026-01-01T00:00:00Z"))
        .stdout(predicate::str::contains("manifest:   sha256:"))
        .stdout(predicate::str::contains(
            "incident-runbook@1 (current, version_id: ver_incident_1) sha256:",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_update_check_json_no_input_never_prints_token() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("stack-child");
    write_skill(&skill, "stack-child", "Use when stack child tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("stack-child", "1", &built.hash);
    let manifest_hash = PackageHash::sha256_of(b"stack manifest");
    let resolve = serde_json::json!({
        "stack": {
            "org": "acme",
            "slug": "engineering-default",
            "name": "Engineering Default",
            "visibility": "org"
        },
        "resolved_at": "2026-01-01T00:00:00Z",
        "manifest_hash": manifest_hash,
        "items": [{
            "skill": "stack-child",
            "version_id": "ver_stack_child_1",
            "version": "1",
            "archive_hash": built.hash.clone(),
            "download": {
                "method": "GET",
                "url": "/v1/orgs/acme/skills/stack-child/versions/1/archive"
            },
            "version_policy": "current"
        }]
    });
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(resolve.clone()),
        json_response(metadata.clone()),
        bytes_response("application/gzip", built.bytes.clone()),
        json_response(whoami_json()),
        json_response(resolve),
    ]);
    login(&cfg, &token_file, &url, "stackupdatesecret1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "stack",
            "install",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "--json",
            "--no-input",
            "stack",
            "update",
            "engineering-default",
            "--target",
            "local",
            "--check",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(!stdout.contains("stackupdatesecret1234"));
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["kind"].as_str(), Some("stack"));
    assert_eq!(json["check"].as_bool(), Some(true));
    assert_eq!(json["updated"].as_bool(), Some(false));
    assert_eq!(json["added"].as_array().unwrap().len(), 0);
    assert_eq!(json["removed"].as_array().unwrap().len(), 0);
    assert_eq!(json["changed"].as_array().unwrap().len(), 0);

    let requests = handle.join().unwrap();
    assert!(
        requests[5].starts_with("GET /v1/orgs/acme/stacks/engineering-default/resolve "),
        "{}",
        requests[5]
    );
}

#[test]
fn skill_export_rejects_removed_version_flag() {
    let (_tmp, cfg, token_file) = fresh_env();
    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "export", "acme/x@1.0.0", "--version", "2.0.0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--version"));
}

#[test]
fn skill_export_allow_yanked_requires_pinned_ref_before_registry_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "export",
            "acme/yanked-export",
            "--allow-yanked",
            "--out",
            ".",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--allow-yanked requires an explicit pinned ref",
        ))
        .stderr(predicate::str::contains("no registry configured").not());
}

#[test]
fn json_export_existing_dir_error_omits_prose_next_command() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("export-conflict");
    write_skill(
        &skill,
        "export-conflict",
        "Use when export conflict tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("export-conflict", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("out");
    out.child("export-conflict").create_dir_all().unwrap();
    out.child("export-conflict")
        .child("keep.txt")
        .write_str("existing")
        .unwrap();

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "export",
            "acme/export-conflict",
            "--out",
            out.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("destination_exists"));
    assert_eq!(json["error"]["action"].as_str(), Some("export"));
    assert!(
        json["error"]["resource"]
            .as_str()
            .unwrap()
            .ends_with("out/export-conflict")
    );
    assert!(json["error"].get("next_command").is_none());
    assert!(json["error"].get("next_command_template").is_none());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn skill_export_quiet_suppresses_next_steps_but_keeps_primary_output() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("quiet-export");
    write_skill(
        &skill,
        "quiet-export",
        "Use when quiet export tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("quiet-export", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "export", "acme/quiet-export@1", "--out"])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("exported acme/quiet-export@1"))
        .stdout(predicate::str::contains("destination:").not())
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("agentstack install").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/quiet-export/versions/1 "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/skills/quiet-export/versions/1/archive "),
        "{}",
        requests[1]
    );
}

#[test]
fn skill_export_rejects_malformed_metadata_version_before_archive_request() {
    let (tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"not requested");
    let metadata = remote_metadata("bad-version", "1.0.0", &hash);
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported");

    cmd(&cfg, &token_file)
        .args(["skill", "export", "acme/bad-version", "--out"])
        .arg(out.path())
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "registry version `1.0.0` must be a positive integer",
        ));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/bad-version "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_export_dry_run_uses_quiet_next_action_copy() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("dry-export");
    write_skill(&skill, "dry-export", "Use when dry export tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("dry-export", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported");

    cmd(&cfg, &token_file)
        .args(["skill", "export", "acme/dry-export@1", "--dry-run", "--out"])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("would export acme/dry-export@1"))
        .stdout(predicate::str::contains(
            "archive verified (hash matches metadata).",
        ))
        .stdout(predicate::str::contains(
            "next: rerun without --dry-run to unpack",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/dry-export/versions/1 "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/skills/dry-export/versions/1/archive "),
        "{}",
        requests[1]
    );
}

#[test]
fn skill_export_json_splits_concrete_and_template_next_commands() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("json-export");
    write_skill(&skill, "json-export", "Use when json export tasks come up");
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("json-export", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "skillexportjsonsecret1234");
    let out = tmp.child("exported-json");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "export", "acme/json-export@1", "--out"])
        .arg(out.path())
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: Value = serde_json::from_str(&stdout).unwrap();
    let destination = json["destination"].as_str().unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/json-export@1"));
    assert!(destination.ends_with("exported-json/json-export"));
    let validate_next = format!("agentstack skill validate {destination}");
    let install_next_template = format!("agentstack skill install {destination} --target <target>");
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some(validate_next.as_str())
    );
    assert!(!json["next_commands"][0].as_str().unwrap().contains('<'));
    assert_eq!(
        json["next_command_templates"][0].as_str(),
        Some(install_next_template.as_str())
    );
    assert!(!stdout.contains("skillexportjsonsecret1234"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/json-export/versions/1 "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_export_dry_run_quiet_suppresses_summary_details() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("quiet-dry-export");
    write_skill(
        &skill,
        "quiet-dry-export",
        "Use when quiet dry-run export tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let metadata = remote_metadata("quiet-dry-export", "1", &built.hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported");

    cmd(&cfg, &token_file)
        .args([
            "--quiet",
            "skill",
            "export",
            "acme/quiet-dry-export@1",
            "--dry-run",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "would export acme/quiet-dry-export@1",
        ))
        .stdout(predicate::str::contains("destination:").not())
        .stdout(predicate::str::contains("version:").not())
        .stdout(predicate::str::contains("hash:").not())
        .stdout(predicate::str::contains("archive verified").not())
        .stdout(predicate::str::contains("next:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/quiet-dry-export/versions/1 "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/skills/quiet-dry-export/versions/1/archive "),
        "{}",
        requests[1]
    );
}

#[test]
fn search_emits_clean_error_against_unreachable_registry() {
    let (_tmp, cfg, token_file) = fresh_env();
    let url = unused_registry_url();
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let output = cmd(&cfg, &token_file)
        .args(["skill", "search", "code-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(url.as_str()))
        .stderr(predicate::str::contains("secrettokenvalue1234").not())
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("registry request failed") || stderr.contains("search "),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn json_registry_unavailable_error_has_ping_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let url = unused_registry_url();
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "search", "code-review"])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("registry_unavailable"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack registry ping")
    );
}

#[test]
fn search_quiet_preserves_results_without_chatter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1.0.0",
                "description": "Use when reviewing code",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "search", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("note:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=review "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_quiet_empty_result_suppresses_next_action() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "search", "missing", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no skills matched `missing` in org `acme`.",
        ))
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("Push one with:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing&org=acme "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_human_output_distinguishes_current_and_latest() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "sql-review",
                "latest_version": "2",
                "current_version": "1",
                "description": "Use when reviewing SQL",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "sql"])
        .assert()
        .success()
        .stdout(predicate::str::contains("showing 1 matches."))
        .stdout(predicate::str::contains("Showing").not())
        .stdout(predicate::str::contains("SKILL"))
        .stdout(predicate::str::contains("CURRENT"))
        .stdout(predicate::str::contains("LATEST"))
        .stdout(predicate::str::contains("acme/sql-review"))
        .stdout(predicate::str::contains("v1 approved"))
        .stdout(predicate::str::contains("v2 not current"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=sql "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_human_output_moves_long_description_below_row() {
    let (_tmp, cfg, token_file) = fresh_env();
    let long_description = "Use when exercising AgentStack catalog search beta flows, including publish, approve, install, update, audit, and search rehearsals for example users";
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "example",
                "name": "catalog-search-beta",
                "latest_version": "1",
                "current_version": "1",
                "description": long_description,
                "visibility": "org",
                "owner_email": "owner@example.com"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["skill", "search", "catalog"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("SKILL"));
    assert!(stdout.contains("OWNER"));
    assert!(stdout.contains("example/catalog-search-beta"));
    assert!(
        stdout.contains("  description: Use when exercising AgentStack catalog search beta flows")
    );
    assert!(stdout.contains("..."));
    assert!(!stdout.contains(long_description));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=catalog "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_empty_result_suggests_listing_same_org() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "missing", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no skills matched `missing` in org `acme`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill list --org acme",
        ))
        .stdout(predicate::str::contains("Push one with:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing&org=acme "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_empty_result_respects_private_visibility_filter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "skill", "search", "missing", "--org", "acme", "--scope", "private",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no skills matched `missing` in org `acme`, scope `private`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill search missing --org acme",
        ))
        .stdout(predicate::str::contains("agentstack skill push").not())
        .stdout(predicate::str::contains("--scope org").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing&org=acme&visibility=private "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_filters_are_sent_and_echoed_in_json() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "code-review",
                "owner_email": "owner@example.com",
                "latest_version": "2",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "platform_tags": ["codex"],
                "updated_at": "2026-05-06T17:42:11Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "search",
            "review",
            "--org",
            "acme",
            "--platform",
            "codex",
            "--platform",
            "claude-code",
            "--scope",
            "org",
            "--owner",
            "owner@example.com",
            "--sort",
            "updated",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["query"].as_str(), Some("review"));
    assert_eq!(json["filters"]["org"].as_str(), Some("acme"));
    assert_eq!(json["filters"]["platform"].as_array().unwrap().len(), 2);
    assert_eq!(json["filters"]["visibility"].as_str(), Some("org"));
    assert_eq!(json["filters"]["owner"].as_str(), Some("owner@example.com"));
    assert_eq!(json["filters"]["sort"].as_str(), Some("updated"));
    assert_eq!(
        json["results"][0]["updated_at"].as_str(),
        Some("2026-05-06T17:42:11Z")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with(
            "GET /v1/search?q=review&org=acme&platform=codex&platform=claude-code&visibility=org&owner=owner%40example.com&sort=updated "
        ),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_search_limit_is_sent_and_echoed_in_json() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json", "skill", "search", "review", "--org", "acme", "--limit", "25",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["filters"]["limit"].as_u64(), Some(25));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=review&org=acme&limit=25 "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_list_filters_limit_and_owner_are_visible_in_json() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "owner_email": "owner@example.com",
                "latest_version": "2",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "platform_tags": ["codex"],
                "updated_at": "2026-05-06T17:42:11Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "list",
            "--org",
            "acme",
            "--platform",
            "codex",
            "--scope",
            "org",
            "--owner",
            "owner@example.com",
            "--sort",
            "owner",
            "--limit",
            "50",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["filters"]["org"].as_str(), Some("acme"));
    assert_eq!(json["filters"]["platform"].as_array().unwrap().len(), 1);
    assert_eq!(json["filters"]["visibility"].as_str(), Some("org"));
    assert_eq!(json["filters"]["owner"].as_str(), Some("owner@example.com"));
    assert_eq!(json["filters"]["sort"].as_str(), Some("owner"));
    assert_eq!(json["filters"]["limit"].as_u64(), Some(50));
    assert_eq!(
        json["skills"][0]["owner_email"].as_str(),
        Some("owner@example.com")
    );
    assert_eq!(
        json["skills"][0]["updated_at"].as_str(),
        Some("2026-05-06T17:42:11Z")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills?platform=codex&visibility=org&owner=owner%40example.com&sort=owner&limit=50 "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_show_renders_installs_line_when_present() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"code-review-bytes");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = "approved".into();
    metadata["current"] = true.into();
    metadata["install_count"] = 42.into();
    metadata["last_installed_at"] = "2026-06-01T12:00:00Z".into();
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review@1"))
        .stdout(predicate::str::contains(
            "installs:   42 (last 2026-06-01T12:00:00Z)",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_show_omits_installs_line_when_server_omits_metrics() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"code-review-bytes");
    let metadata = remote_metadata("code-review", "1", &hash);
    let (url, handle) = registry_server(vec![
        json_response(metadata.clone()),
        json_response(metadata),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "show", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review@1"))
        .stdout(predicate::str::contains("installs:").not());

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/code-review"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert!(json.get("install_count").is_none());
    assert!(json.get("last_installed_at").is_none());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn skill_show_json_passes_install_metrics_through() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"code-review-bytes");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["install_count"] = 42.into();
    metadata["last_installed_at"] = "2026-06-01T12:00:00Z".into();
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "show", "acme/code-review"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["install_count"].as_u64(), Some(42));
    assert_eq!(
        json["last_installed_at"].as_str(),
        Some("2026-06-01T12:00:00Z")
    );

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
}

#[test]
fn skill_list_renders_installs_column_when_any_row_has_metrics() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "owner_email": "owner@example.com",
                "latest_version": "2",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "install_count": 12,
                "last_installed_at": "2026-06-01T12:00:00Z"
            },
            {
                "org": "acme",
                "name": "format-md",
                "owner_email": "owner@example.com",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when formatting markdown",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSTALLS"))
        .stdout(predicate::str::contains("owner@example.com  12"))
        .stdout(predicate::str::contains("owner@example.com  -"))
        .stdout(predicate::str::contains(
            "description: Use when reviewing code",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_list_omits_installs_column_when_server_omits_metrics() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("SKILL"))
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("INSTALLS").not());

    handle.join().unwrap();
}

#[test]
fn skill_list_json_passes_install_metrics_through() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "install_count": 12,
                "last_installed_at": "2026-06-01T12:00:00Z"
            },
            {
                "org": "acme",
                "name": "format-md",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when formatting markdown",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "list", "--org", "acme"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skills"][0]["install_count"].as_u64(), Some(12));
    assert_eq!(
        json["skills"][0]["last_installed_at"].as_str(),
        Some("2026-06-01T12:00:00Z")
    );
    assert!(json["skills"][1].get("install_count").is_none());
    assert!(json["skills"][1].get("last_installed_at").is_none());

    handle.join().unwrap();
}

#[test]
fn skill_list_sort_installs_is_sent_to_registry() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme", "--sort", "installs"])
        .assert()
        .success();

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills?sort=installs "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_search_renders_installs_column_when_any_row_has_metrics() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "install_count": 12,
                "last_installed_at": "2026-06-01T12:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("INSTALLS"))
        .stdout(predicate::str::contains("org         -      12"))
        .stdout(predicate::str::contains(
            "description: Use when reviewing code",
        ));

    handle.join().unwrap();
}

#[test]
fn skill_search_omits_installs_column_when_server_omits_metrics() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "search", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("INSTALLS").not());

    handle.join().unwrap();
}

#[test]
fn skill_search_sort_installs_is_sent_and_metrics_pass_through_json() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1",
                "current_version": "1",
                "description": "Use when reviewing code",
                "visibility": "org",
                "install_count": 12,
                "last_installed_at": "2026-06-01T12:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "search", "review", "--sort", "installs"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["filters"]["sort"].as_str(), Some("installs"));
    assert_eq!(json["results"][0]["install_count"].as_u64(), Some(12));
    assert_eq!(
        json["results"][0]["last_installed_at"].as_str(),
        Some("2026-06-01T12:00:00Z")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=review&sort=installs "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_rejects_bad_ref() {
    let (_tmp, cfg, token_file) = fresh_env();
    login(
        &cfg,
        &token_file,
        "https://registry.example.com",
        "secrettokenvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "Bad_Name"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid skill name"));
}

#[test]
fn versions_without_token_errors_before_registry_call() {
    let (_tmp, cfg, token_file) = fresh_env();
    set_registry(&cfg, &token_file, "https://registry.example.com");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "acme/code-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("registry request failed").not());
}

#[test]
fn versions_empty_result_prints_empty_state() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains(
            "no uploaded versions found for `acme/code-review`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill list --org acme",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_json_empty_result_includes_empty_state_with_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "version", "list", "acme/code-review"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/code-review"));
    assert!(json["versions"].as_array().unwrap().is_empty());
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no uploaded versions found for `acme/code-review`.")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill list --org acme")
    );
    let next_command = json["next_command"].as_str().unwrap();
    assert!(
        next_command.starts_with("agentstack ")
            && !(next_command.contains('<') || next_command.contains('>')),
        "next_command must be concrete for JSON: {next_command}"
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_quiet_preserves_results_without_chatter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"quiet-version");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": [
            {
                "version": "1.0.0",
                "hash": hash,
                "created_at": "2026-01-01T00:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "version", "list", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("1.0.0"))
        .stdout(predicate::str::contains("agentstack approve").not())
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("note:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_quiet_suppresses_install_current_guidance() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"quiet-current-version");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": [
            {
                "version": "1",
                "hash": hash,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "version", "list", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("current: v1 approved"))
        .stdout(predicate::str::contains("install current:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_human_output_prints_install_current_guidance() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"current-version");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": [
            {
                "version": "1",
                "hash": hash,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "acme/code-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("current: v1 approved"))
        .stdout(predicate::str::contains(
            "install current: agentstack skill install acme/code-review --target <target>",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/code-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_human_output_explains_missing_current() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"candidate-version");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": [
            {
                "version": "1",
                "hash": hash,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "candidate",
                "current": false
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "acme/sql-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("current: none"))
        .stdout(predicate::str::contains(
            "no approved current version yet; ask an admin to run:",
        ))
        .stdout(predicate::str::contains(
            "agentstack skill version approve acme/sql-review@1",
        ))
        .stdout(predicate::str::contains("latest:  v1 candidate"))
        .stdout(predicate::str::contains("VERSION"))
        .stdout(predicate::str::contains("CURRENT"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/sql-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn versions_human_output_omits_install_current_for_yanked_current() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"yanked-current-version");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "versions": [
            {
                "version": "1",
                "hash": hash,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true,
                "yanked_at": "2026-01-02T00:00:00Z",
                "yank_reason": "bad archive"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "list", "acme/sql-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("current: v1 yanked"))
        .stdout(predicate::str::contains("yanked: bad archive"))
        .stdout(predicate::str::contains("install current:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/sql-review/versions "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_status_human_output_names_current_candidate_visibility_and_next() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash_current = PackageHash::sha256_of(b"status-current");
    let hash_candidate = PackageHash::sha256_of(b"status-candidate");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skill": {
            "org": "acme",
            "name": "sql-review",
            "latest_version": "2",
            "current_version": "1",
            "description": "Use when SQL review tasks come up",
            "visibility": "org"
        },
        "versions": [
            {
                "version": "2",
                "hash": hash_candidate,
                "created_at": "2026-01-02T00:00:00Z",
                "status": "candidate",
                "current": false
            },
            {
                "version": "1",
                "hash": hash_current,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "status", "acme/sql-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("visibility:       org"))
        .stdout(predicate::str::contains("current version:  1"))
        .stdout(predicate::str::contains("latest upload:    2 (candidate)"))
        .stdout(predicate::str::contains(
            "org or team admin can approve with `agentstack skill version approve acme/sql-review@2`",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/sql-review/status "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_status_json_candidate_includes_approve_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash_current = PackageHash::sha256_of(b"status-current");
    let hash_candidate = PackageHash::sha256_of(b"status-candidate");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skill": {
            "org": "acme",
            "name": "sql-review",
            "latest_version": "2",
            "current_version": "1",
            "description": "Use when SQL review tasks come up",
            "visibility": "org"
        },
        "versions": [
            {
                "version": "2",
                "hash": hash_candidate,
                "created_at": "2026-01-02T00:00:00Z",
                "status": "candidate",
                "current": false
            },
            {
                "version": "1",
                "hash": hash_current,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "status", "acme/sql-review"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill"]["name"].as_str(), Some("sql-review"));
    assert_eq!(json["versions"].as_array().unwrap().len(), 2);
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill version approve acme/sql-review@2")
    );
    assert!(json.get("next_command_template").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/sql-review/status "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_status_json_current_includes_install_next_command_template() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash_current = PackageHash::sha256_of(b"status-current");
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skill": {
            "org": "acme",
            "name": "sql-review",
            "latest_version": "1",
            "current_version": "1",
            "description": "Use when SQL review tasks come up",
            "visibility": "org"
        },
        "versions": [
            {
                "version": "1",
                "hash": hash_current,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "approved",
                "current": true
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "status", "acme/sql-review"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill"]["name"].as_str(), Some("sql-review"));
    assert_eq!(json["versions"].as_array().unwrap().len(), 1);
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack skill install acme/sql-review --target <target>")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/sql-review/status "),
        "{}",
        requests[0]
    );
}

#[test]
fn audit_human_output_shows_actor_resource_and_show_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "events": [
            {
                "id": "aud_123",
                "org": "acme",
                "action": "skill.version_approved",
                "resource_type": "skill",
                "resource_id": "skill_1",
                "resource": "acme/sql-review",
                "actor_email": "admin@example.com",
                "metadata": { "version": "1" },
                "created_at": "2026-01-01T00:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["audit", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("audit events: 1"))
        .stdout(predicate::str::contains("skill.version_approved"))
        .stdout(predicate::str::contains("admin@example.com"))
        .stdout(predicate::str::contains("acme/sql-review"))
        .stdout(predicate::str::contains(
            "agentstack audit show <EVENT_ID> --org acme",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/audit "),
        "{}",
        requests[0]
    );
}

#[test]
fn audit_list_omits_org_when_active_token_has_one_org() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "events": []
        })),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["audit", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no audit events"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/audit "),
        "{}",
        requests[1]
    );
}

#[test]
fn audit_list_json_includes_template_next_command_when_events_exist() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "events": [
            {
                "id": "aud_123",
                "org": "acme",
                "action": "skill.version_approved",
                "resource_type": "skill",
                "resource_id": "skill_1",
                "resource": "acme/sql-review",
                "actor_email": "admin@example.com",
                "metadata": { "version": "1" },
                "created_at": "2026-01-01T00:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "audit", "list", "--org", "acme"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["events"].as_array().unwrap().len(), 1);
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack audit show <EVENT_ID> --org acme")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/audit "),
        "{}",
        requests[0]
    );
}

#[test]
fn audit_list_json_empty_omits_next_command_template() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "events": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "audit", "list", "--org", "acme"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert!(json["events"].as_array().unwrap().is_empty());
    assert!(json.get("next_command").is_none());
    assert!(json.get("next_command_template").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/audit "),
        "{}",
        requests[0]
    );
}

#[test]
fn approve_posts_promote_request() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"approved-version");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["audit_event_id"] = serde_json::json!("aud_approve_1");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "version", "approve", "acme/code-review@1"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "approved acme/code-review@1 as current",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("Next commands").not())
        .stdout(predicate::str::contains(
            "agentstack skill status acme/code-review",
        ))
        .stdout(predicate::str::contains(
            "agentstack skill install acme/code-review@1 --target <target>",
        ))
        .stdout(predicate::str::contains(
            "agentstack audit show aud_approve_1 --org acme",
        ));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/approve "),
        "{}",
        requests[0]
    );
}

#[test]
fn approve_rejects_missing_version_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["skill", "version", "approve", "code-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "approve expects `skill@version` or `org/skill@version`",
        ))
        .stderr(predicate::str::contains("no registry configured").not())
        .stderr(predicate::str::contains("not logged in").not());
}

#[test]
fn yank_rejects_missing_version_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "version",
            "yank",
            "code-review",
            "--reason",
            "bad archive",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "skill ref must include `@<version>`",
        ))
        .stderr(predicate::str::contains("no registry configured").not())
        .stderr(predicate::str::contains("not logged in").not());
}

#[test]
fn approve_quiet_suppresses_success_output() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"quiet-approve");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "--quiet",
            "skill",
            "version",
            "approve",
            "acme/code-review@1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/approve "),
        "{}",
        requests[0]
    );
}

#[test]
fn repeated_approve_json_keeps_audit_event_id() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"approved-version");
    let mut metadata_one = remote_metadata("code-review", "1", &hash);
    metadata_one["status"] = serde_json::json!("approved");
    metadata_one["current"] = serde_json::json!(true);
    metadata_one["audit_event_id"] = serde_json::json!("aud_approve_1");
    let mut metadata_two = metadata_one.clone();
    metadata_two["audit_event_id"] = serde_json::json!("aud_approve_2");
    let (url, handle) = registry_server(vec![
        json_response(metadata_one),
        json_response(metadata_two),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    for expected in ["aud_approve_1", "aud_approve_2"] {
        let assert = cmd(&cfg, &token_file)
            .args([
                "--json",
                "skill",
                "version",
                "approve",
                "acme/code-review@1",
            ])
            .assert()
            .success();
        let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
        assert_eq!(json["audit_event_id"].as_str(), Some(expected));
    }

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request.starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/approve ")
    }));
}

#[test]
fn approve_json_includes_post_approval_next_guidance() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"approved-next-guidance");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["audit_event_id"] = serde_json::json!("aud_approve_next");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "approvenextsecret1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "version",
            "approve",
            "acme/code-review@1",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/code-review@1"));
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_approve_next"));
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill status acme/code-review")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack audit show aud_approve_next --org acme")
    );
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );
    assert_eq!(
        json["next_command_templates"][0].as_str(),
        Some("agentstack skill install acme/code-review@1 --target <target>")
    );
    assert!(!stdout.contains("approvenextsecret1234"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/approve "),
        "{}",
        requests[0]
    );
}

#[test]
fn repeated_visibility_set_json_keeps_audit_event_id() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![
        json_response(serde_json::json!({
            "org": "acme",
            "skill": "code-review",
            "visibility": "org",
            "audit_event_id": "aud_visibility_1"
        })),
        json_response(serde_json::json!({
            "org": "acme",
            "skill": "code-review",
            "visibility": "org",
            "audit_event_id": "aud_visibility_2"
        })),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    for expected in ["aud_visibility_1", "aud_visibility_2"] {
        let assert = cmd(&cfg, &token_file)
            .args([
                "--json",
                "skill",
                "visibility",
                "set",
                "acme/code-review",
                "--scope",
                "org",
            ])
            .assert()
            .success();
        let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
        assert_eq!(json["audit_event_id"].as_str(), Some(expected));
    }

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests.iter().all(
            |request| request.starts_with("PATCH /v1/orgs/acme/skills/code-review/visibility ")
        )
    );
}

#[test]
fn skill_visibility_set_team_sends_team_and_echoes_it() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "org": "acme",
        "skill": "code-review",
        "visibility": "team",
        "team": "engineering",
        "audit_event_id": "aud_visibility_team"
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "visibility",
            "set",
            "acme/code-review",
            "--scope",
            "team",
            "--team",
            "engineering",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["visibility"].as_str(), Some("team"));
    assert_eq!(json["team"].as_str(), Some("engineering"));
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_visibility_team"));

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("PATCH /v1/orgs/acme/skills/code-review/visibility "),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains(r#""visibility":"team""#),
        "{}",
        requests[0]
    );
    assert!(
        requests[0].contains(r#""team":"engineering""#),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_visibility_set_validates_team_args_before_registry_config_lookup() {
    let (_tmp, cfg, token_file) = fresh_env();

    for (args, expected) in [
        (
            vec![
                "skill",
                "visibility",
                "set",
                "acme/code-review",
                "--scope",
                "team",
            ],
            "--team is required",
        ),
        (
            vec![
                "skill",
                "visibility",
                "set",
                "acme/code-review",
                "--scope",
                "org",
                "--team",
                "engineering",
            ],
            "--team can only be used with --scope team",
        ),
        (
            vec![
                "skill",
                "visibility",
                "set",
                "acme/code-review",
                "--scope",
                "team",
                "--team",
                "Engineering",
            ],
            "invalid --team",
        ),
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains(expected))
            .stderr(predicate::str::contains("no registry configured").not());
    }
}

#[test]
fn yank_posts_reason_to_lifecycle_endpoint() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"yanked-version");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["yanked_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["yank_reason"] = serde_json::json!("bad archive");
    metadata["audit_event_id"] = serde_json::json!("aud_yank_1");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "version",
            "yank",
            "acme/code-review@1",
            "--reason",
            "bad archive",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/code-review@1"));
    assert_eq!(json["action"].as_str(), Some("yanked"));
    assert_eq!(
        json["metadata"]["yank_reason"].as_str(),
        Some("bad archive")
    );
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_yank_1"));
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill status acme/code-review")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack audit show aud_yank_1 --org acme")
    );
    assert!(json.get("next_command_templates").is_none());
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/yank "),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains("\"reason\":\"bad archive\""));
}

#[test]
fn yank_quiet_suppresses_success_output() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"quiet-yank");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["yanked_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["yank_reason"] = serde_json::json!("bad archive");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "--quiet",
            "skill",
            "version",
            "yank",
            "acme/code-review@1",
            "--reason",
            "bad archive",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/yank "),
        "{}",
        requests[0]
    );
}

#[test]
fn deprecate_posts_reason_to_lifecycle_endpoint() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"deprecated-version");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["deprecated_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["deprecation_reason"] = serde_json::json!("superseded");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "version",
            "deprecate",
            "acme/code-review@1",
            "--reason",
            "superseded",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("deprecated acme/code-review@1"))
        .stdout(predicate::str::contains("reason:     superseded"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/deprecate "),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains("\"reason\":\"superseded\""));
}

#[test]
fn deprecate_json_includes_lifecycle_next_guidance() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"deprecated-json-guidance");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["deprecated_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["deprecation_reason"] = serde_json::json!("superseded");
    metadata["audit_event_id"] = serde_json::json!("aud_deprecate_1");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "version",
            "deprecate",
            "acme/code-review@1",
            "--reason",
            "superseded",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["skill_ref"].as_str(), Some("acme/code-review@1"));
    assert_eq!(json["action"].as_str(), Some("deprecated"));
    assert_eq!(
        json["metadata"]["deprecation_reason"].as_str(),
        Some("superseded")
    );
    assert_eq!(json["audit_event_id"].as_str(), Some("aud_deprecate_1"));
    assert_eq!(
        json["next_commands"][0].as_str(),
        Some("agentstack skill status acme/code-review")
    );
    assert_eq!(
        json["next_commands"][1].as_str(),
        Some("agentstack audit show aud_deprecate_1 --org acme")
    );
    assert!(json.get("next_command_templates").is_none());
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/deprecate "),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains("\"reason\":\"superseded\""));
}

#[test]
fn deprecate_quiet_suppresses_success_output() {
    let (_tmp, cfg, token_file) = fresh_env();
    let hash = PackageHash::sha256_of(b"quiet-deprecate");
    let mut metadata = remote_metadata("code-review", "1", &hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["deprecated_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["deprecation_reason"] = serde_json::json!("superseded");
    let (url, handle) = registry_server(vec![json_response(metadata)]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "--quiet",
            "skill",
            "version",
            "deprecate",
            "acme/code-review@1",
            "--reason",
            "superseded",
        ])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("POST /v1/orgs/acme/skills/code-review/versions/1/deprecate "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_export_allow_yanked_appends_archive_query() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("yanked-export");
    write_skill(
        &skill,
        "yanked-export",
        "Use when yanked export tasks come up",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let mut metadata = remote_metadata("yanked-export", "1", &built.hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["yanked_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["yank_reason"] = serde_json::json!("bad archive");
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        bytes_response("application/gzip", built.bytes),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported-yanked");

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "export",
            "acme/yanked-export@1",
            "--allow-yanked",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("exported acme/yanked-export@1"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills/yanked-export/versions/1 "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with(
            "GET /v1/orgs/acme/skills/yanked-export/versions/1/archive?allow_yanked=true "
        ),
        "{}",
        requests[1]
    );
}

#[test]
fn skill_export_allow_yanked_forbidden_explains_server_admin_recovery() {
    let (tmp, cfg, token_file) = fresh_env();
    let skill = tmp.child("yanked-forbidden");
    write_skill(
        &skill,
        "yanked-forbidden",
        "Use when yanked recovery is denied",
    );
    let built = build_skill_package(skill.path()).unwrap();
    let mut metadata = remote_metadata("yanked-forbidden", "1", &built.hash);
    metadata["status"] = serde_json::json!("approved");
    metadata["current"] = serde_json::json!(true);
    metadata["yanked_at"] = serde_json::json!("2026-01-01T00:00:00Z");
    metadata["yank_reason"] = serde_json::json!("bad archive");
    let (url, handle) = registry_server(vec![
        json_response(metadata),
        json_status_response(
            "403 Forbidden",
            serde_json::json!({
                "error": {
                    "code": "forbidden",
                    "message": "permission denied",
                    "http_status": 403
                }
            }),
        ),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    let out = tmp.child("exported-yanked");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "export",
            "acme/yanked-forbidden@1",
            "--allow-yanked",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("forbidden"));
    assert_eq!(json["error"]["http_status"].as_u64(), Some(403));
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only server admins can recover yanked archives")
    );
    assert!(json["error"].get("next_command").is_none());

    let requests = handle.join().unwrap();
    assert!(
        requests[1].starts_with(
            "GET /v1/orgs/acme/skills/yanked-forbidden/versions/1/archive?allow_yanked=true "
        ),
        "{}",
        requests[1]
    );
}

#[test]
fn update_all_rejects_skill_name_conflict() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["install", "update", "alpha", "--all"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn update_single_requires_target_before_registry_lookup() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "update", "alpha"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--target <TARGET>"))
        .stderr(predicate::str::contains("registry request failed").not());
}

#[test]
fn update_check_json_previews_file_changes() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("preview-skill");
    write_skill(
        &skill,
        "preview-skill",
        "Use when preview skill tasks come up",
    );
    let built_v1 = build_skill_package(skill.path()).unwrap();

    // Modified v2 archive: one added file and a changed SKILL.md.
    skill.child("references").create_dir_all().unwrap();
    skill
        .child("references/new.md")
        .write_str("new note\n")
        .unwrap();
    let body = std::fs::read_to_string(skill.child("SKILL.md").path()).unwrap();
    std::fs::write(
        skill.child("SKILL.md").path(),
        format!("{body}\nUpdated guidance.\n"),
    )
    .unwrap();
    let built_v2 = build_skill_package(skill.path()).unwrap();

    let (url, handle) = registry_server(vec![
        // skill install acme/preview-skill@1
        json_response(whoami_json()),
        json_response(remote_metadata("preview-skill", "1", &built_v1.hash)),
        bytes_response("application/gzip", built_v1.bytes.clone()),
        // skill update --check downloads the new archive for the preview.
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "versions": [
                {
                    "version": "2",
                    "hash": built_v2.hash.clone(),
                    "created_at": "2026-01-02T00:00:00Z",
                    "status": "approved",
                    "current": true
                },
                {
                    "version": "1",
                    "hash": built_v1.hash.clone(),
                    "created_at": "2026-01-01T00:00:00Z",
                    "status": "approved"
                }
            ]
        })),
        json_response(remote_metadata("preview-skill", "2", &built_v2.hash)),
        bytes_response("application/gzip", built_v2.bytes.clone()),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "install",
            "acme/preview-skill@1",
            "--target",
            "local",
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "--json",
            "skill",
            "update",
            "preview-skill",
            "--target",
            "local",
            "--check",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["update_available"], true);
    assert_eq!(json["updated"], false);
    assert_eq!(json["changes"]["added"][0], "references/new.md");
    assert_eq!(json["changes"]["removed"].as_array().unwrap().len(), 0);
    assert_eq!(json["changes"]["changed"][0], "SKILL.md");
    assert!(json.get("changes_error").is_none());

    // The check must not modify the installed copy.
    let receipt_text =
        std::fs::read_to_string(dest.child("preview-skill/.agentstack-install.json").path())
            .unwrap();
    let receipt: Value = serde_json::from_str(&receipt_text).unwrap();
    assert_eq!(receipt["version"].as_str(), Some("1"));
    assert!(
        !dest
            .child("preview-skill/references/new.md")
            .path()
            .exists()
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[6].starts_with("GET /v1/orgs/acme/skills/preview-skill/versions/2/archive "),
        "{}",
        requests[6]
    );
}

#[test]
fn update_check_json_degrades_when_preview_download_fails() {
    let (tmp, cfg, token_file) = fresh_env();
    let cache = tmp.child("cache");
    let dest = tmp.child("target");
    let skill = tmp.child("preview-skill");
    write_skill(
        &skill,
        "preview-skill",
        "Use when preview skill tasks come up",
    );
    let built_v1 = build_skill_package(skill.path()).unwrap();
    let hash_v2 = PackageHash::sha256_of(b"unreachable-v2-archive");

    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(remote_metadata("preview-skill", "1", &built_v1.hash)),
        bytes_response("application/gzip", built_v1.bytes.clone()),
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "versions": [
                {
                    "version": "2",
                    "hash": hash_v2,
                    "created_at": "2026-01-02T00:00:00Z",
                    "status": "approved",
                    "current": true
                }
            ]
        })),
        json_status_response(
            "500 Internal Server Error",
            serde_json::json!({ "error": { "message": "archive store offline" } }),
        ),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            dest.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "skill",
            "install",
            "acme/preview-skill@1",
            "--target",
            "local",
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .env("AGENTSTACK_CACHE_DIR", cache.path())
        .args([
            "--json",
            "skill",
            "update",
            "preview-skill",
            "--target",
            "local",
            "--check",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    // The version delta is still reported; only the preview is missing.
    assert_eq!(json["update_available"], true);
    assert_eq!(json["latest_version"], "2");
    assert!(json.get("changes").is_none());
    assert!(json["changes_error"].is_string());

    handle.join().unwrap();
}

#[test]
fn skill_diff_target_conflicts_with_right_positional() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "diff", "left", "right", "--target", "local"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

#[test]
fn skill_diff_requires_right_or_target() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "diff", "only-left"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

#[test]
fn json_skill_diff_target_before_install_error_has_receipt_fields() {
    let (tmp, cfg, token_file) = fresh_env();
    let target = tmp.child("target");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "diff",
            "missing-skill",
            "--target",
            "local",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("diff"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack install list --target local")
    );
}

#[test]
fn json_update_before_install_error_has_receipt_fields() {
    let (tmp, cfg, token_file) = fresh_env();
    let target = tmp.child("target");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    login(
        &cfg,
        &token_file,
        &unused_registry_url(),
        "secrettokenvalue1234",
    );

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "update",
            "missing-skill",
            "--target",
            "local",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("update"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack install list --target local")
    );
}

#[test]
fn json_skill_show_before_install_error_uses_concrete_list_command() {
    let (tmp, cfg, token_file) = fresh_env();
    let target = tmp.child("target");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "show",
            "missing-skill",
            "--target",
            "local",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("show"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack install list --target local")
    );
}

#[test]
fn json_stack_show_before_install_error_uses_show_action() {
    let (tmp, cfg, token_file) = fresh_env();
    let target = tmp.child("target");
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "show",
            "acme/engineering-default",
            "--target",
            "local",
        ])
        .assert()
        .failure();
    assert!(assert.get_output().stdout.is_empty());
    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("install_receipt_missing")
    );
    assert_eq!(json["error"]["action"].as_str(), Some("show"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack stack install acme/engineering-default --target local")
    );
}

#[test]
fn stack_export_without_name_prints_actionable_example() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "export"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("<STACK_REF>"));
}

#[test]
fn update_all_json_empty_is_single_document() {
    let (tmp, cfg, token_file) = fresh_env();

    let assert = cmd(&cfg, &token_file)
        .env("HOME", tmp.path())
        .current_dir(tmp.path())
        .args(["--json", "install", "update", "--all"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["batch"].as_bool(), Some(true));
    assert_eq!(json["check"].as_bool(), Some(false));
    assert_eq!(json["force"].as_bool(), Some(false));
    assert_eq!(json["results"].as_array().unwrap().len(), 0);
    assert_eq!(json["summary"]["skipped"].as_u64(), Some(0));
    assert_eq!(json["summary"]["failed"].as_u64(), Some(0));
}

#[test]
fn update_all_hints_when_only_stack_receipts_exist() {
    let (_tmp, cfg, token_file) = fresh_env();
    let target = assert_fs::TempDir::new().unwrap();
    cmd(&cfg, &token_file)
        .args([
            "target",
            "set",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    write_stack_receipt(
        target.path(),
        &StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack_install".to_string(),
            org: "acme".to_string(),
            stack: "engineering-default".to_string(),
            registry_url: Some("https://registry.example.com/v1".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: PackageHash::sha256_of(b"stack manifest"),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:01Z".to_string(),
            installed_by: Some("tester".to_string()),
            items: Vec::new(),
        },
    )
    .unwrap();

    cmd(&cfg, &token_file)
        .args(["install", "update", "--all", "--target", "local", "--check"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no direct skill install receipts found for target `local`",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("Next command:").not())
        .stdout(predicate::str::contains(
            "agentstack stack update acme/engineering-default --target local",
        ));
}

#[test]
fn list_remote_org_only_with_remote() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));
}

#[test]
fn read_only_registry_commands_without_token_error_before_registry_call() {
    let (_tmp, cfg, token_file) = fresh_env();
    set_registry(&cfg, &token_file, "https://registry.example.com");

    for args in [
        &["skill", "export", "acme/code-review", "--out", "."][..],
        &["skill", "search", "code-review"],
        &["skill", "version", "list", "acme/code-review"],
        &["skill", "list"],
    ] {
        cmd(&cfg, &token_file)
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("not logged in"))
            .stderr(predicate::str::contains("registry request failed").not());
    }
}

#[test]
fn list_remote_against_unreachable_registry_errors_with_url() {
    let (_tmp, cfg, token_file) = fresh_env();
    let url = unused_registry_url();
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let output = cmd(&cfg, &token_file)
        .args(["skill", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(url.as_str()))
        .stderr(predicate::str::contains("secrettokenvalue1234").not())
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("registry request failed") || stderr.contains("list "),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn list_remote_quiet_preserves_results_without_chatter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "latest_version": "1.0.0",
                "description": "Use when reviewing code",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("acme/code-review"))
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("note:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_quiet_empty_result_suppresses_next_action() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["--quiet", "skill", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no skills found in `acme`."))
        .stdout(predicate::str::contains("next:").not())
        .stdout(predicate::str::contains("Push one with:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_human_output_distinguishes_current_and_latest() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "sql-review",
                "latest_version": "2",
                "current_version": "1",
                "description": "Use when reviewing SQL",
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("showing 1 skills (org=acme)"))
        .stdout(predicate::str::contains("Showing").not())
        .stdout(predicate::str::contains("SKILL"))
        .stdout(predicate::str::contains("CURRENT"))
        .stdout(predicate::str::contains("LATEST"))
        .stdout(predicate::str::contains("acme/sql-review"))
        .stdout(predicate::str::contains("v1 approved"))
        .stdout(predicate::str::contains("v2 not current"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_human_output_moves_long_description_below_row() {
    let (_tmp, cfg, token_file) = fresh_env();
    let long_description = "Use when exercising AgentStack catalog list alpha flows, including publish, approve, install, update, audit, and search rehearsals for example users";
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "example",
                "name": "catalog-list-alpha",
                "latest_version": "1",
                "current_version": "1",
                "description": long_description,
                "visibility": "org",
                "owner_email": "owner@example.com"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "example"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("SKILL"));
    assert!(stdout.contains("OWNER"));
    assert!(stdout.contains("example/catalog-list-alpha"));
    assert!(
        stdout.contains("  description: Use when exercising AgentStack catalog list alpha flows")
    );
    assert!(stdout.contains("..."));
    assert!(!stdout.contains(long_description));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/example/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_json_preserves_full_description() {
    let (_tmp, cfg, token_file) = fresh_env();
    let long_description = "Use when exercising AgentStack catalog list alpha flows, including publish, approve, install, update, audit, and search rehearsals for example users";
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "example",
                "name": "catalog-list-alpha",
                "latest_version": "1",
                "current_version": "1",
                "description": long_description,
                "visibility": "org"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "list", "--org", "example"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["skills"][0]["description"].as_str(),
        Some(long_description)
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/example/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_empty_result_suggests_searching_same_org() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no skills found in `acme`."))
        .stdout(predicate::str::contains(
            "next: agentstack skill search <query> --org acme",
        ))
        .stdout(predicate::str::contains("Push one with:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_empty_result_respects_team_filter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["skill", "list", "--org", "acme", "--team", "engineering"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no skills found in `acme` matching team `engineering`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill list --org acme",
        ))
        .stdout(predicate::str::contains("agentstack skill push").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills?team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_list_json_empty_result_includes_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "list",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no skills found in `acme` matching team `engineering`.")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill list --org acme")
    );
    let next_command = json["next_command"].as_str().unwrap();
    assert!(
        next_command.starts_with("agentstack ")
            && !(next_command.contains('<') || next_command.contains('>')),
        "next_command must be concrete for JSON: {next_command}"
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills?team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_list_json_empty_template_suggestion_includes_template_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "skill", "list", "--org", "acme"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no skills found in `acme`.")
    );
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack skill search <query> --org acme")
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills "),
        "{}",
        requests[0]
    );
}

#[test]
fn search_empty_result_respects_team_filter() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "skill",
            "search",
            "missing",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no skills matched `missing` in org `acme`, team `engineering`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack skill search missing --org acme",
        ))
        .stdout(predicate::str::contains("agentstack skill push").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing&org=acme&team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn skill_search_json_empty_result_includes_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "results": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "search",
            "missing",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no skills matched `missing` in org `acme`, team `engineering`.")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack skill search missing --org acme")
    );
    let next_command = json["next_command"].as_str().unwrap();
    assert!(
        next_command.starts_with("agentstack ")
            && !(next_command.contains('<') || next_command.contains('>')),
        "next_command must be concrete for JSON: {next_command}"
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/search?q=missing&org=acme&team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_list_omits_org_when_active_token_has_one_org() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![
        json_response(whoami_json()),
        json_response(serde_json::json!({
            "stacks": []
        })),
    ]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["stack", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stacks found in `acme`."));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/whoami "),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/orgs/acme/stacks "),
        "{}",
        requests[1]
    );
}

#[test]
fn stack_list_empty_result_suggests_broader_stack_list() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "stacks": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["stack", "list", "--org", "acme", "--team", "engineering"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no stacks found in `acme` matching team `engineering`.",
        ))
        .stdout(predicate::str::contains(
            "next: agentstack stack list --org acme",
        ))
        .stdout(predicate::str::contains("agentstack stack create").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks?team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_list_quiet_empty_result_suppresses_next_action() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "stacks": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args([
            "--quiet",
            "stack",
            "list",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no stacks found in `acme` matching team `engineering`.",
        ))
        .stdout(predicate::str::contains("next:").not());

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks?team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_list_human_output_uses_quiet_summary_copy() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "stacks": [
            {
                "org": "acme",
                "slug": "engineering-default",
                "name": "engineering-default",
                "description": "Shared engineering defaults",
                "visibility": "team",
                "item_count": 2,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    cmd(&cfg, &token_file)
        .args(["stack", "list", "--org", "acme"])
        .assert()
        .success()
        .stdout(predicate::str::contains("showing 1 stack(s) (org=acme)"))
        .stdout(predicate::str::contains("Showing").not())
        .stdout(predicate::str::contains("acme/engineering-default"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks "),
        "{}",
        requests[0]
    );
}

#[test]
fn stack_list_json_empty_result_includes_filters_and_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "stacks": []
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "stack",
            "list",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["filters"]["team"].as_str(), Some("engineering"));
    assert_eq!(
        json["empty_message"].as_str(),
        Some("no stacks found in `acme` matching team `engineering`.")
    );
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack stack list --org acme")
    );
    let next_command = json["next_command"].as_str().unwrap();
    assert!(
        next_command.starts_with("agentstack ")
            && !(next_command.contains('<') || next_command.contains('>')),
        "next_command must be concrete for JSON: {next_command}"
    );

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/stacks?team=engineering "),
        "{}",
        requests[0]
    );
}

#[test]
fn list_remote_team_filter_is_sent_and_echoed_in_json() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = registry_server(vec![json_response(serde_json::json!({
        "skills": [
            {
                "org": "acme",
                "name": "code-review",
                "team": "engineering",
                "latest_version": "1.0.0",
                "description": "Use when reviewing code",
                "visibility": "team"
            }
        ]
    }))]);
    login(&cfg, &token_file, &url, "secrettokenvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args([
            "--json",
            "skill",
            "list",
            "--org",
            "acme",
            "--team",
            "engineering",
        ])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["org"].as_str(), Some("acme"));
    assert_eq!(json["team"].as_str(), Some("engineering"));
    assert_eq!(json["skills"][0]["team"].as_str(), Some("engineering"));

    let requests = handle.join().unwrap();
    assert!(
        requests[0].starts_with("GET /v1/orgs/acme/skills?team=engineering "),
        "{}",
        requests[0]
    );
}
