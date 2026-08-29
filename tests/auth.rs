//! Integration tests for the auth/registry CLI surface.
//!
//! We isolate every test from the real keyring + global config by setting:
//!   - `AGENTSTACK_CONFIG_DIR` -> temp dir (config.toml lives here)
//!   - `AGENTSTACK_TOKEN_FILE` -> temp file (file-backed token store)
//!   - `AGENTSTACK_TOKEN`      -> unset by default (env override)
//!
//! Tests in this file mutate environment-derived state through the
//! AgentStack binary, but the binary is launched as a subprocess by
//! assert_cmd, so each invocation gets its own env.

use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;
use std::io::{Read, Write};
use url::Url;

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
        .env_remove("AGENTSTACK_TOKEN_PATH")
        .env_remove("AGENTSTACK_REGISTRY_URL")
        .env_remove("AGENTSTACK_NONINTERACTIVE")
        .env_remove("CI");
    c
}

fn unused_registry_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    format!("http://{addr}")
}

fn whoami_server(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    let (url, handle) = whoami_server_n(body, 1);
    let handle = std::thread::spawn(move || {
        let mut requests = handle.join().unwrap();
        requests.pop().unwrap()
    });
    (url, handle)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
        .unwrap_or(request.len());
    let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len().saturating_sub(header_end) < content_length {
        let n = stream.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn http_body(request: &str) -> &str {
    request.split("\r\n\r\n").nth(1).unwrap_or_default()
}

fn write_json_response(stream: &mut std::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).unwrap();
}

fn send_oauth_callback(redirect_uri: &str, code: &str, state: &str) -> String {
    send_oauth_callback_target(redirect_uri, &format!("code={code}&state={state}"))
}

fn send_oauth_callback_target(redirect_uri: &str, query: &str) -> String {
    let url = Url::parse(redirect_uri).unwrap();
    let host = url.host_str().unwrap();
    let port = url.port().unwrap();
    let mut stream = std::net::TcpStream::connect((host, port)).unwrap();
    let target = format!("{}?{query}", url.path());
    let request =
        format!("GET {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn google_authorization_url() -> &'static str {
    "https://accounts.google.com/o/oauth2/v2/auth?flow=test"
}

fn whoami_server_n(
    body: &'static str,
    count: usize,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..count {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());
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

fn scoped_account_for_url(url: &str) -> String {
    let base = url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("registry:{base}/:account:default")
    } else {
        format!("registry:{base}/v1/:account:default")
    }
}

fn seed_token(token_file: &std::path::Path, url: &str, token: &str) {
    std::fs::write(
        token_file,
        serde_json::json!({ scoped_account_for_url(url): token }).to_string(),
    )
    .unwrap();
}

fn whoami_status_server(
    status_line: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (url, handle)
}

fn ping_auth_server() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in [r#"{"status":"ok","server_version":"0.1.0"}"#, whoami_body()] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            requests.push(String::from_utf8_lossy(&buf[..n]).into_owned());
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

fn public_ping_server() -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = r#"{"status":"ok","server_version":"0.1.0"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (url, handle)
}

fn whoami_body() -> &'static str {
    r#"{
        "user": "pilot@example.com",
        "org": "demo",
        "email": "pilot@example.com",
        "name": "Pilot User",
        "server_admin": true,
        "orgs": [
            { "slug": "demo", "name": "Demo", "role": "org_admin" },
            { "slug": "team", "name": "Team", "role": "reader" }
        ]
    }"#
}

fn whoami_single_org_body() -> &'static str {
    r#"{
        "user": "pilot@example.com",
        "org": "acme",
        "email": "pilot@example.com",
        "name": "Pilot User",
        "server_admin": false,
        "orgs": [
            { "slug": "acme", "name": "Acme", "role": "org_admin" }
        ]
    }"#
}

#[test]
fn whoami_says_not_logged_in_when_no_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));
}

#[test]
fn registry_use_then_show_round_trips() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["registry", "use", "https://registry.example.com"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "registry set to https://registry.example.com",
        ));

    cmd(&cfg, &token_file)
        .args(["registry", "show"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("https://registry.example.com"));
}

#[test]
fn registry_set_notes_when_registry_url_env_is_active() {
    let (_tmp, cfg, token_file) = fresh_env();
    let mut c = cmd(&cfg, &token_file);
    c.env(
        "AGENTSTACK_REGISTRY_URL",
        "https://env.registry.example.com",
    )
    .args(["registry", "use", "https://saved.registry.example.com"])
    .assert()
    .success()
    .stdout(predicate::str::contains(
        "registry set to https://saved.registry.example.com",
    ))
    .stdout(predicate::str::contains(
        "note: saved to config, but AGENTSTACK_REGISTRY_URL is currently active",
    ));
}

#[test]
fn registry_set_rejects_bad_url() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["registry", "use", "ftp://example.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must start with http://"));

    cmd(&cfg, &token_file)
        .args(["registry", "use", " https://x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("whitespace"));
}

#[test]
fn login_rejects_removed_server_and_token_flags() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--server", "https://registry.example.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--server"));

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token", "compatserversecret1234"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--token"))
        .stderr(predicate::str::contains("compatserversecret1234").not());
}

#[test]
fn login_token_stdin_stores_token_without_printing_it() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("stdinsecretvalue5678\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("logged in to {url}")))
        .stdout(predicate::str::contains("stdinsecretvalue5678").not())
        .stdout(predicate::str::contains("***5678").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("stdinsecretvalue5678"));
    let _ = handle.join().unwrap();
}

#[test]
fn login_token_stdin_prints_single_org_next_step() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_single_org_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("singleorgsecret1234\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("next: agentstack skill list"));

    let _ = handle.join().unwrap();
}

#[test]
fn login_token_stdin_prints_multiple_orgs_next_step_placeholder() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("multiorgsecret1234\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("orgs: demo, team"))
        .stdout(predicate::str::contains(
            "next: agentstack skill list --org <org>",
        ));

    let _ = handle.join().unwrap();
}

#[test]
fn login_json_multiple_orgs_includes_template_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "auth", "login", "--token-stdin"])
        .write_stdin("multiorgjsonsecret1234\n")
        .assert()
        .success();
    let json: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert!(json.get("next_command").is_none());
    assert_eq!(
        json["next_command_template"].as_str(),
        Some("agentstack skill list --org <org>")
    );
    assert!(
        !String::from_utf8_lossy(&assert.get_output().stdout).contains("multiorgjsonsecret1234")
    );

    let _ = handle.join().unwrap();
}

#[test]
fn login_quiet_suppresses_next_step_but_logs_in() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_single_org_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["--quiet", "auth", "login", "--token-stdin"])
        .write_stdin("quietsecret1234\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("logged in to {url}")))
        .stdout(predicate::str::contains("next:").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("quietsecret1234"));
    let _ = handle.join().unwrap();
}

#[test]
fn login_oauth_no_browser_exchanges_code_validates_and_stores_agentstack_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut start_stream, _) = listener.accept().unwrap();
        let start_request = read_http_request(&mut start_stream);
        assert!(
            start_request.starts_with("POST /v1/auth/oauth/start "),
            "{start_request}"
        );
        assert!(
            !start_request
                .to_ascii_lowercase()
                .contains("authorization:"),
            "OAuth start must not send bearer auth: {start_request}"
        );
        let start_json: Value = serde_json::from_str(http_body(&start_request)).unwrap();
        assert_eq!(start_json["provider"].as_str(), Some("google"));
        assert_eq!(start_json["code_challenge_method"].as_str(), Some("S256"));
        assert_eq!(start_json["client"].as_str(), Some("agentstack-cli"));
        assert!(start_json["code_challenge"].as_str().unwrap().len() >= 43);
        let redirect_uri = start_json["redirect_uri"].as_str().unwrap().to_string();
        assert!(redirect_uri.starts_with("http://127.0.0.1:"));
        assert!(redirect_uri.ends_with("/auth/callback"));
        let state = start_json["state"].as_str().unwrap().to_string();
        assert!(state.len() >= 43);
        let auth_url = google_authorization_url();
        write_json_response(
            &mut start_stream,
            &serde_json::json!({
                "authorization_url": auth_url,
                "state": state,
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        let callback_response =
            send_oauth_callback(&redirect_uri, "agentstack-login-code-123", &state);
        assert!(callback_response.starts_with("HTTP/1.1 200 OK"));
        assert!(callback_response.contains("You are signed in to AgentStack"));
        assert!(!callback_response.contains("login complete"));

        let (mut token_stream, _) = listener.accept().unwrap();
        let token_request = read_http_request(&mut token_stream);
        assert!(
            token_request.starts_with("POST /v1/auth/oauth/token "),
            "{token_request}"
        );
        assert!(
            !token_request
                .to_ascii_lowercase()
                .contains("authorization:"),
            "OAuth token exchange must not send bearer auth: {token_request}"
        );
        let token_json: Value = serde_json::from_str(http_body(&token_request)).unwrap();
        assert_eq!(
            token_json["grant_type"].as_str(),
            Some("authorization_code")
        );
        assert_eq!(
            token_json["code"].as_str(),
            Some("agentstack-login-code-123")
        );
        assert_eq!(
            token_json["redirect_uri"].as_str(),
            Some(redirect_uri.as_str())
        );
        assert!(token_json["code_verifier"].as_str().unwrap().len() >= 43);
        write_json_response(
            &mut token_stream,
            r#"{"token_type":"Bearer","access_token":"oauthagentstacktoken1234","expires_at":"2026-07-04 12:00:00+00","identity":{"user":"pilot@example.com","email":"pilot@example.com","name":"Pilot User"}}"#,
        );

        let (mut whoami_stream, _) = listener.accept().unwrap();
        let whoami_request = read_http_request(&mut whoami_stream);
        assert!(
            whoami_request.starts_with("GET /v1/whoami "),
            "{whoami_request}"
        );
        assert!(
            whoami_request
                .to_ascii_lowercase()
                .contains("authorization: bearer oauthagentstacktoken1234"),
            "{whoami_request}"
        );
        write_json_response(&mut whoami_stream, whoami_single_org_body());

        (start_request, token_request, whoami_request)
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .success()
        .stdout(predicate::str::contains("open this URL to continue:"))
        .stdout(predicate::str::contains("logged in to"))
        .stdout(predicate::str::contains("auth:  oauth_browser"))
        .stdout(predicate::str::contains("oauthagentstacktoken1234").not())
        .stdout(predicate::str::contains("***1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("oauthagentstacktoken1234"));
    let (_start, token_request, _whoami) = handle.join().unwrap();
    assert!(
        !token_request.contains("oauthagentstacktoken1234"),
        "raw AgentStack token must only appear in the token response and credential store"
    );
}

#[test]
fn login_oauth_rejects_authorization_url_outside_registry_origin() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let start_json: Value = serde_json::from_str(http_body(&request)).unwrap();
        write_json_response(
            &mut stream,
            &serde_json::json!({
                "authorization_url": "https://evil.example.com/oauth",
                "state": start_json["state"].as_str().unwrap(),
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        request
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingoauthgoodtoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "outside the expected registry or provider origin",
        ))
        .stderr(predicate::str::contains("existingoauthgoodtoken1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingoauthgoodtoken1234"));
    let request = handle.join().unwrap();
    assert!(request.starts_with("POST /v1/auth/oauth/start "));
}

#[test]
fn login_oauth_state_mismatch_does_not_replace_existing_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let start_json: Value = serde_json::from_str(http_body(&request)).unwrap();
        let redirect_uri = start_json["redirect_uri"].as_str().unwrap().to_string();
        write_json_response(
            &mut stream,
            &serde_json::json!({
                "authorization_url": google_authorization_url(),
                "state": start_json["state"].as_str().unwrap(),
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        send_oauth_callback(&redirect_uri, "agentstack-login-code-123", "wrong-state");
        request
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingstatetoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("callback state did not match"))
        .stderr(predicate::str::contains("existingstatetoken1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingstatetoken1234"));
    let request = handle.join().unwrap();
    assert!(request.starts_with("POST /v1/auth/oauth/start "));
}

#[test]
fn login_oauth_callback_error_redacts_description() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        let start_json: Value = serde_json::from_str(http_body(&request)).unwrap();
        let redirect_uri = start_json["redirect_uri"].as_str().unwrap().to_string();
        let state = start_json["state"].as_str().unwrap().to_string();
        write_json_response(
            &mut stream,
            &serde_json::json!({
                "authorization_url": google_authorization_url(),
                "state": state,
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        let callback_response = send_oauth_callback_target(
            &redirect_uri,
            &format!(
                "error=access_denied&error_description=token%3Dcallbacksecret1234&state={state}"
            ),
        );
        assert!(callback_response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(callback_response.contains("AgentStack login failed"));
        request
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingcallbackgoodtoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "OAuth login failed: access_denied",
        ))
        .stderr(predicate::str::contains("callbacksecret1234").not())
        .stderr(predicate::str::contains("token=").not())
        .stderr(predicate::str::contains("existingcallbackgoodtoken1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingcallbackgoodtoken1234"));
    let request = handle.join().unwrap();
    assert!(request.starts_with("POST /v1/auth/oauth/start "));
}

#[test]
fn login_oauth_rejects_registry_replaced_state_without_waiting_for_callback() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        write_json_response(
            &mut stream,
            &serde_json::json!({
                "authorization_url": google_authorization_url(),
                "state": "server-replaced-state",
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        request
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingreturnedstatetoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("state"))
        .stderr(predicate::str::contains("server-replaced-state").not())
        .stderr(predicate::str::contains("existingreturnedstatetoken1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingreturnedstatetoken1234"));
    let request = handle.join().unwrap();
    assert!(request.starts_with("POST /v1/auth/oauth/start "));
}

#[test]
fn login_oauth_whoami_rejection_does_not_store_exchanged_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut start_stream, _) = listener.accept().unwrap();
        let start_request = read_http_request(&mut start_stream);
        let start_json: Value = serde_json::from_str(http_body(&start_request)).unwrap();
        let redirect_uri = start_json["redirect_uri"].as_str().unwrap().to_string();
        let state = start_json["state"].as_str().unwrap().to_string();
        write_json_response(
            &mut start_stream,
            &serde_json::json!({
                "authorization_url": google_authorization_url(),
                "state": state,
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        send_oauth_callback(&redirect_uri, "agentstack-login-code-456", &state);

        let (mut token_stream, _) = listener.accept().unwrap();
        let token_request = read_http_request(&mut token_stream);
        write_json_response(
            &mut token_stream,
            r#"{"token_type":"Bearer","access_token":"oauthwhoamibadtoken1234"}"#,
        );

        let (mut whoami_stream, _) = listener.accept().unwrap();
        let whoami_request = read_http_request(&mut whoami_stream);
        let body = r#"{"error":{"code":"unauthenticated","message":"token invalid"}}"#;
        let response = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        whoami_stream.write_all(response.as_bytes()).unwrap();
        (token_request, whoami_request)
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingwhoamigoodtoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed with HTTP 401"))
        .stderr(predicate::str::contains("oauthwhoamibadtoken1234").not())
        .stderr(predicate::str::contains("existingwhoamigoodtoken1234").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingwhoamigoodtoken1234"));
    assert!(!tokens.contains("oauthwhoamibadtoken1234"));
    let (_token_request, whoami_request) = handle.join().unwrap();
    assert!(
        whoami_request
            .to_ascii_lowercase()
            .contains("authorization: bearer oauthwhoamibadtoken1234"),
        "{whoami_request}"
    );
}

#[test]
fn login_oauth_exchange_error_redacts_code_verifier_and_preserves_oauth_code() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut start_stream, _) = listener.accept().unwrap();
        let start_request = read_http_request(&mut start_stream);
        let start_json: Value = serde_json::from_str(http_body(&start_request)).unwrap();
        let redirect_uri = start_json["redirect_uri"].as_str().unwrap().to_string();
        let state = start_json["state"].as_str().unwrap().to_string();
        write_json_response(
            &mut start_stream,
            &serde_json::json!({
                "authorization_url": google_authorization_url(),
                "state": state,
                "expires_in_seconds": 300
            })
            .to_string(),
        );
        send_oauth_callback(&redirect_uri, "agentstack-login-code-789", &state);

        let (mut token_stream, _) = listener.accept().unwrap();
        let token_request = read_http_request(&mut token_stream);
        let token_json: Value = serde_json::from_str(http_body(&token_request)).unwrap();
        let verifier = token_json["code_verifier"].as_str().unwrap();
        let body = serde_json::json!({
            "error": {
                "code": "oauth_invalid_grant",
                "message": format!("bad code_verifier={verifier}")
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        token_stream.write_all(response.as_bytes()).unwrap();
        verifier.to_string()
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingexchangegoodtoken1234");

    let assert = cmd(&cfg, &token_file)
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("open this URL to continue:"))
        .stderr(predicate::str::contains("oauth_invalid_grant"))
        .stderr(predicate::str::contains("code_verifier=[REDACTED]"));

    let verifier = handle.join().unwrap();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        !stderr.contains(&verifier),
        "stderr leaked verifier: {stderr}"
    );

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingexchangegoodtoken1234"));
}

#[test]
fn login_reads_piped_stdin_without_flag() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login"])
        .write_stdin("implicitstdinsecret9012\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("logged in to {url}")))
        .stdout(predicate::str::contains("implicitstdinsecret9012").not())
        .stdout(predicate::str::contains("***9012").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("implicitstdinsecret9012"));
    let _ = handle.join().unwrap();
}

#[test]
fn switching_registry_does_not_reuse_previous_registry_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (first_url, first_handle) = whoami_server(whoami_body());
    let (second_url, second_handle) = public_ping_server();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &first_url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("firstregistrysecret1234\n")
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &second_url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["registry", "ping", "--auth"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"))
        .stderr(predicate::str::contains("firstregistrysecret1234").not());

    let first_request = first_handle.join().unwrap();
    assert!(
        first_request
            .to_ascii_lowercase()
            .contains("authorization: bearer firstregistrysecret1234"),
        "{first_request}"
    );

    let second_request = second_handle.join().unwrap();
    assert!(
        second_request.starts_with("GET /v1/ping "),
        "{second_request}"
    );
    assert!(
        !second_request
            .to_ascii_lowercase()
            .contains("authorization:"),
        "switched registry must not receive previous token: {second_request}"
    );
    assert!(!second_request.contains("firstregistrysecret1234"));
}

#[test]
fn login_reports_reauth_replacement_without_printing_tokens() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server_n(whoami_body(), 2);

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login"])
        .write_stdin("oldreauthsecret1234\n")
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login"])
        .write_stdin("newreauthsecret5678\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("replaced existing stored token"))
        .stdout(predicate::str::contains("oldreauthsecret1234").not())
        .stdout(predicate::str::contains("newreauthsecret5678").not())
        .stdout(predicate::str::contains("***1234").not())
        .stdout(predicate::str::contains("***5678").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(!tokens.contains("oldreauthsecret1234"));
    assert!(tokens.contains("newreauthsecret5678"));
    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn login_rejects_remote_invalid_token_without_replacing_existing_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_status_server(
        "401 Unauthorized",
        r#"{"error":{"code":"badtokenvalue5678_code","message":"badtokenvalue5678 is invalid"}}"#,
    );

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existinggoodtoken1234");

    cmd(&cfg, &token_file)
        .args(["auth", "login"])
        .write_stdin("badtokenvalue5678\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("login validation against"))
        .stderr(predicate::str::contains("HTTP 401"))
        .stderr(predicate::str::contains("badtokenvalue5678").not())
        .stderr(predicate::str::contains("***5678").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existinggoodtoken1234"));
    assert!(!tokens.contains("badtokenvalue5678"));

    let request = handle.join().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer badtokenvalue5678"),
        "{request}"
    );
}

#[test]
fn login_json_rejects_remote_invalid_token_without_leaking_error_body() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) =
        whoami_status_server("401 Unauthorized", "malicious echo jsonbadtokenvalue2468");

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingjsongoodtoken1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "auth", "login"])
        .write_stdin("jsonbadtokenvalue2468\n")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("login_validation_failed"))
        .stderr(predicate::str::contains("jsonbadtokenvalue2468").not())
        .stderr(predicate::str::contains("***2468").not());

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let json: Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(
        json["error"]["code"].as_str(),
        Some("login_validation_failed")
    );
    assert_eq!(json["error"]["http_status"].as_u64(), Some(401));
    assert!(
        json["error"]["causes"].as_array().unwrap().is_empty(),
        "{json}"
    );

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingjsongoodtoken1234"));
    assert!(!tokens.contains("jsonbadtokenvalue2468"));

    let request = handle.join().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer jsonbadtokenvalue2468"),
        "{request}"
    );
}

#[test]
fn login_json_preserves_structured_unauthenticated_code_without_token_leak() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_status_server(
        "401 Unauthorized",
        r#"{"error":{"code":"unauthenticated","message":"token=structuredbadtoken1357 is invalid"}}"#,
    );

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "existingstructuregoodtoken1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "auth", "login"])
        .write_stdin("structuredbadtoken1357\n")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("structuredbadtoken1357").not())
        .stderr(predicate::str::contains("***1357").not());

    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("unauthenticated"));
    assert_eq!(json["error"]["action"].as_str(), Some("auth login"));
    assert_eq!(
        json["error"]["next_command"].as_str(),
        Some("agentstack auth login")
    );
    assert_eq!(json["error"]["http_status"].as_u64(), Some(401));

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("existingstructuregoodtoken1234"));
    assert!(!tokens.contains("structuredbadtoken1357"));

    let request = handle.join().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer structuredbadtoken1357"),
        "{request}"
    );
}

#[test]
fn login_validates_supplied_token_even_when_env_token_is_set() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_TOKEN", "envoverridevalue9876")
        .args(["auth", "login"])
        .write_stdin("logininputvalue1234\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN is set"))
        .stdout(predicate::str::contains("logininputvalue1234").not())
        .stdout(predicate::str::contains("envoverridevalue9876").not());

    let tokens = std::fs::read_to_string(&token_file).unwrap();
    assert!(tokens.contains("logininputvalue1234"));
    assert!(!tokens.contains("envoverridevalue9876"));

    let request = handle.join().unwrap();
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer logininputvalue1234"),
        "{request}"
    );
    assert!(!request.contains("envoverridevalue9876"), "{request}");
}

#[test]
fn login_token_stdin_rejects_internal_whitespace() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["registry", "use", "https://registry.example.com"])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("first-line-token\nsecond-line\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("internal whitespace"))
        .stderr(predicate::str::contains("first-line-token").not())
        .stderr(predicate::str::contains("second-line").not());

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("tok with space\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains("internal whitespace"))
        .stderr(predicate::str::contains("tok with space").not());
}

#[test]
fn login_implicit_stdin_rejects_internal_whitespace_without_echoing_token() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["registry", "use", "https://registry.example.com"])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login"])
        .write_stdin("implicit token with spaces\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "stdin token input must not contain internal whitespace",
        ))
        .stderr(predicate::str::contains("implicit token with spaces").not());
}

#[test]
fn registry_ping_public_sends_no_auth_header_when_token_is_stored() {
    let (_tmp, cfg, token_file) = fresh_env();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = r#"{"status":"ok","server_version":"0.1.0"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();
    seed_token(&token_file, &url, "storedpingsecret1234");

    cmd(&cfg, &token_file)
        .args(["registry", "ping"])
        .assert()
        .success()
        .stdout(predicate::str::contains("storedpingsecret1234").not());

    let request = handle.join().unwrap();
    assert!(request.starts_with("GET /v1/ping "), "{request}");
    assert!(
        !request.to_ascii_lowercase().contains("authorization:"),
        "public ping must not send auth: {request}"
    );
    assert!(
        !request.contains("storedpingsecret1234"),
        "stored token must never appear in public ping request: {request}"
    );
}

#[test]
fn login_token_stdin_rejects_empty_input() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("\n")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--token-stdin input must not be empty",
        ));
}

#[test]
fn login_no_input_empty_stdin_teaches_auth_paths() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["--no-input", "auth", "login"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "humans run `agentstack auth login`",
        ))
        .stderr(predicate::str::contains("AGENTSTACK_TOKEN_PATH"))
        .stderr(predicate::str::contains("stdin token input must not be empty").not());
}

#[test]
fn login_oauth_in_ci_refuses_and_points_at_token_env() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .env("CI", "true")
        .args(["auth", "login", "--no-browser", "--callback-port", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "browser OAuth login requires an interactive terminal",
        ))
        .stderr(predicate::str::contains("AGENTSTACK_TOKEN_PATH"));
}

#[test]
fn login_no_input_json_empty_stdin_teaches_auth_paths() {
    let (_tmp, cfg, token_file) = fresh_env();

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "--no-input", "auth", "login"])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("AGENTSTACK_TOKEN_PATH"))
        .stderr(predicate::str::contains("stdin token input must not be empty").not());

    let json: Value = serde_json::from_slice(assert.get_output().stderr.as_slice()).unwrap();
    assert_eq!(json["error"]["code"].as_str(), Some("token_required"));
    assert_eq!(json["error"]["action"].as_str(), Some("auth login"));
    assert_eq!(json["error"]["status"].as_str(), Some("missing_token"));
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
            .any(|method| method.as_str() == Some("AGENTSTACK_TOKEN"))
    );
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("humans run `agentstack auth login`")
    );
}

#[test]
fn whoami_calls_remote_server_when_configured() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server_n(whoami_body(), 2);

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("remotesecretvalue1234\n")
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged in"))
        .stdout(predicate::str::contains(format!("server:       {url}")))
        .stdout(predicate::str::contains("email:        pilot@example.com"))
        .stdout(predicate::str::contains("name:         Pilot User"))
        .stdout(predicate::str::contains("server_admin: true"))
        .stdout(predicate::str::contains("demo (Demo) role=org_admin"))
        .stdout(predicate::str::contains("remotesecretvalue1234").not())
        .stdout(predicate::str::contains("***1234").not());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    let request = &requests[1];
    assert!(request.starts_with("GET /v1/whoami "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer remotesecretvalue1234"),
        "{request}"
    );
}

#[test]
fn env_registry_url_and_token_support_headless_whoami() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_REGISTRY_URL", &url)
        .env("AGENTSTACK_TOKEN", "headlesssecretvalue1234")
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged in"))
        .stdout(predicate::str::contains(format!("server:       {url}")))
        .stdout(predicate::str::contains("headlesssecretvalue1234").not())
        .stdout(predicate::str::contains("***1234").not());

    let request = handle.join().unwrap();
    assert!(request.starts_with("GET /v1/whoami "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer headlesssecretvalue1234"),
        "{request}"
    );
    assert!(
        !cfg.join("config.toml").exists(),
        "headless whoami must not persist registry config"
    );
}

#[test]
fn env_registry_url_and_token_path_support_headless_whoami() {
    let (tmp, cfg, token_file) = fresh_env();
    let token_path = tmp.child("agentstack-token");
    token_path.write_str("tokenpathsecret1234\n").unwrap();
    let (url, handle) = whoami_server(whoami_body());

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_REGISTRY_URL", &url)
        .env("AGENTSTACK_TOKEN_PATH", token_path.path())
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("logged in"))
        .stdout(predicate::str::contains(
            "source:       AGENTSTACK_TOKEN_PATH",
        ))
        .stdout(predicate::str::contains("tokenpathsecret1234").not())
        .stdout(predicate::str::contains("***1234").not());

    let request = handle.join().unwrap();
    assert!(request.starts_with("GET /v1/whoami "), "{request}");
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer tokenpathsecret1234"),
        "{request}"
    );
    assert!(
        !cfg.join("config.toml").exists(),
        "headless whoami must not persist registry config"
    );
}

#[test]
fn env_registry_url_overrides_configured_registry() {
    let (_tmp, cfg, token_file) = fresh_env();
    let stale = unused_registry_url();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &stale])
        .assert()
        .success();

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_REGISTRY_URL", &url)
        .env("AGENTSTACK_TOKEN", "overrideurlsecret1234")
        .args(["auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("server:       {url}")))
        .stdout(predicate::str::contains(stale).not());

    let request = handle.join().unwrap();
    assert!(request.starts_with("GET /v1/whoami "), "{request}");
}

#[test]
fn invalid_registry_url_env_errors_without_echoing_value() {
    let (_tmp, cfg, token_file) = fresh_env();
    let secret_url = "https://registry.example.com?token=urlsecret1234";

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_REGISTRY_URL", secret_url)
        .env("AGENTSTACK_TOKEN", "envsecretvalue5678")
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "AGENTSTACK_REGISTRY_URL is invalid",
        ))
        .stderr(predicate::str::contains("urlsecret1234").not())
        .stderr(predicate::str::contains("envsecretvalue5678").not())
        .stderr(predicate::str::contains(secret_url).not());
}

#[test]
fn whoami_json_uses_remote_identity_shape() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server_n(whoami_body(), 2);

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("jsonsecretvalue1234\n")
        .assert()
        .success();

    let out = cmd(&cfg, &token_file)
        .args(["--json", "auth", "whoami"])
        .assert()
        .success()
        .stdout(predicate::str::contains("jsonsecretvalue1234").not())
        .stdout(predicate::str::contains("***1234").not())
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["logged_in"], Value::Bool(true));
    assert_eq!(json["server"].as_str(), Some(url.as_str()));
    assert_eq!(json["email"].as_str(), Some("pilot@example.com"));
    assert_eq!(json["name"].as_str(), Some("Pilot User"));
    assert_eq!(json["server_admin"], Value::Bool(true));
    assert_eq!(json["orgs"].as_array().unwrap().len(), 2);

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    let request = &requests[1];
    assert!(request.starts_with("GET /v1/whoami "), "{request}");
}

#[test]
fn registry_use_rejects_bad_url_before_login() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["registry", "use", "not-a-url"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("http://"));
}

#[test]
fn login_without_server_uses_configured_registry() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("anothertokenvalue9876\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("logged in to {url}")));
    let _ = handle.join().unwrap();
}

#[test]
fn login_without_server_uses_registry_url_env_without_persisting_it() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_REGISTRY_URL", &url)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("envlogintokenvalue9876\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("logged in to {url}")))
        .stdout(predicate::str::contains("envlogintokenvalue9876").not());

    assert!(
        !cfg.join("config.toml").exists(),
        "login without --server must not persist AGENTSTACK_REGISTRY_URL"
    );
    let _ = handle.join().unwrap();
}

#[test]
fn whoami_local_reports_default_registry_without_config() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "server: https://registry.agentstack.gg",
        ));
}

#[test]
fn auth_status_reports_local_state_without_network() {
    let (_tmp, cfg, token_file) = fresh_env();
    seed_token(
        &token_file,
        "https://registry.agentstack.gg",
        "storedstatusvalue1234",
    );

    cmd(&cfg, &token_file)
        .args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("local auth status"))
        .stdout(predicate::str::contains(
            "server: https://registry.agentstack.gg",
        ))
        .stdout(predicate::str::contains("token:  present"))
        .stdout(predicate::str::contains("storedstatusvalue1234").not())
        .stdout(predicate::str::contains("***1234").not());
}

#[test]
fn auth_status_json_matches_local_whoami_shape() {
    let (_tmp, cfg, token_file) = fresh_env();
    let assert = cmd(&cfg, &token_file)
        .args(["--json", "auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"logged_in\""))
        .stdout(predicate::str::contains("\"token_present\""));

    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert_eq!(json["logged_in"].as_bool(), Some(false));
    assert_eq!(
        json["server"].as_str(),
        Some("https://registry.agentstack.gg")
    );
    assert_eq!(json["token_present"].as_bool(), Some(false));
    assert_eq!(json["next_command"].as_str(), Some("agentstack auth login"));
    assert!(json.get("next_command_template").is_none());
}

#[test]
fn auth_status_json_when_complete_omits_next_command() {
    let (_tmp, cfg, token_file) = fresh_env();
    seed_token(
        &token_file,
        "https://registry.agentstack.gg",
        "storedstatusjsonvalue9876",
    );

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("storedstatusjsonvalue9876").not());

    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert_eq!(json["logged_in"].as_bool(), Some(true));
    assert_eq!(
        json["server"].as_str(),
        Some("https://registry.agentstack.gg")
    );
    assert_eq!(json["token_present"].as_bool(), Some(true));
    assert!(json.get("next_command").is_none());
    assert!(json.get("next_command_template").is_none());
}

#[test]
fn logout_removes_stored_token() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = whoami_server(whoami_body());

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["auth", "login", "--token-stdin"])
        .write_stdin("tokentoremovexyz\n")
        .assert()
        .success();
    let _ = handle.join().unwrap();

    cmd(&cfg, &token_file)
        .args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed stored token"));

    cmd(&cfg, &token_file)
        .args(["auth", "whoami"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not logged in"));

    // Logging out again is a no-op, not an error.
    cmd(&cfg, &token_file)
        .args(["auth", "logout"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no stored token to remove"));
}

#[test]
fn env_var_overrides_stored_token_in_whoami() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["registry", "use", "https://r.example.com"])
        .assert()
        .success();

    seed_token(&token_file, "https://r.example.com", "storedtokenvalue1234");

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_TOKEN", "envoverridevalue9876");
    c.args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN"))
        .stdout(predicate::str::contains("***9876").not())
        .stdout(predicate::str::contains("envoverridevalue9876").not())
        // Stored token should not appear when env var wins.
        .stdout(predicate::str::contains("***1234").not())
        .stdout(predicate::str::contains("storedtokenvalue1234").not());
}

#[test]
fn whoami_local_reports_env_token_without_printing_it() {
    let (_tmp, cfg, token_file) = fresh_env();

    let mut c = cmd(&cfg, &token_file);
    c.env("AGENTSTACK_TOKEN", "envonlyvalue9876");
    c.args(["auth", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("local auth status"))
        .stdout(predicate::str::contains(
            "server: https://registry.agentstack.gg",
        ))
        .stdout(predicate::str::contains("token:  present"))
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN"))
        .stdout(predicate::str::contains("envonlyvalue9876").not())
        .stdout(predicate::str::contains("***9876").not());
}

#[test]
fn registry_ping_handles_network_errors_cleanly() {
    let (_tmp, cfg, token_file) = fresh_env();
    let url = unused_registry_url();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    seed_token(&token_file, &url, "secretpingvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["registry", "ping"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(url.as_str()))
        .stderr(predicate::str::contains("secretpingvalue1234").not());
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("registry request failed") || stderr.contains("could not reach"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn registry_ping_auth_validates_token_after_public_ping() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = ping_auth_server();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    seed_token(&token_file, &url, "secretpingvalue1234");

    cmd(&cfg, &token_file)
        .args(["registry", "ping", "--auth"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ok:"))
        .stdout(predicate::str::contains("auth: ok (pilot@example.com)"))
        .stdout(predicate::str::contains("secretpingvalue1234").not());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /v1/ping "), "{}", requests[0]);
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("user-agent: agentstack/"),
        "{}",
        requests[0]
    );
    assert!(
        !requests[0].to_ascii_lowercase().contains("authorization:"),
        "{}",
        requests[0]
    );
    assert!(
        requests[1].starts_with("GET /v1/whoami "),
        "{}",
        requests[1]
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: bearer secretpingvalue1234"),
        "{}",
        requests[1]
    );
}

#[test]
fn registry_ping_json_reports_auth_status() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = ping_auth_server();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    seed_token(&token_file, &url, "secretpingvalue1234");

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "registry", "ping", "--auth"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert_eq!(json["url"].as_str(), Some(url.as_str()));
    assert_eq!(json["ok"].as_bool(), Some(true));
    assert_eq!(json["authenticated"].as_bool(), Some(true));
    assert_eq!(json["email"].as_str(), Some("pilot@example.com"));
    assert!(json.get("next_command").is_none());

    let requests = handle.join().unwrap();
    assert_eq!(requests.len(), 2);
}

#[test]
fn registry_ping_json_reports_null_auth_when_not_checked() {
    let (_tmp, cfg, token_file) = fresh_env();
    let (url, handle) = public_ping_server();

    cmd(&cfg, &token_file)
        .args(["registry", "use", &url])
        .assert()
        .success();

    let assert = cmd(&cfg, &token_file)
        .args(["--json", "registry", "ping"])
        .assert()
        .success();
    let json: Value = serde_json::from_slice(assert.get_output().stdout.as_slice()).unwrap();
    assert_eq!(json["ok"].as_bool(), Some(true));
    // `--auth` was not passed, so the token was never checked: the field is
    // `null`, not `false` (which would imply a failed check).
    assert!(json["authenticated"].is_null(), "{json}");
    assert!(json.get("email").is_none() || json["email"].is_null());
    assert_eq!(
        json["next_command"].as_str(),
        Some("agentstack registry ping --auth")
    );

    let request = handle.join().unwrap();
    assert!(request.starts_with("GET /v1/ping "), "{request}");
}

#[test]
fn registry_ping_failure_points_to_doctor_instead_of_itself() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["registry", "use", "http://127.0.0.1:9"])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["registry", "ping"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("next: agentstack doctor"))
        .stderr(predicate::str::contains("next: agentstack registry ping").not());
}

#[test]
fn registry_show_without_override_reports_default() {
    let (_tmp, cfg, token_file) = fresh_env();
    cmd(&cfg, &token_file)
        .args(["registry", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "https://registry.agentstack.gg (default)",
        ));
}

#[test]
fn config_show_includes_registry_section() {
    let (_tmp, cfg, token_file) = fresh_env();

    cmd(&cfg, &token_file)
        .args(["registry", "use", "https://r.example.com"])
        .assert()
        .success();

    cmd(&cfg, &token_file)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[registry]"))
        .stdout(predicate::str::contains("https://r.example.com"));
}
