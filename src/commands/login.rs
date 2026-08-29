use std::io::{self, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use rand::RngCore as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

use super::client::configured_registry_url;
use crate::config::ConfigStore;
use crate::credentials::{
    DEFAULT_ACCOUNT, Token, env_token_present, normalize_token_material, scoped_account,
    token_store,
};
use crate::error::CliError;
use crate::output::Ctx;
use crate::registry::{
    HttpRegistryClient, OAuthExchangeRequest, OAuthStartRequest, OrgMembership, RegistryClient,
    RegistryConnection, WhoamiResponse, validate_registry_url,
};

const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 49152;

pub struct Args {
    pub token_stdin: bool,
    pub provider: String,
    pub no_browser: bool,
    pub callback_port: Option<u16>,
    pub timeout_seconds: u64,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let store = ConfigStore::load().context("failed to load config")?;
    let url = configured_registry_url(&store)?.url;
    validate_registry_url(&url)?;

    let login = read_or_request_token(ctx, &url, &args)?;

    let token = login.token;
    let identity = validate_login_token(&url, token.clone())?;

    let token_store = token_store();
    let account = scoped_account(&url, DEFAULT_ACCOUNT)?;
    let org_slugs = identity
        .orgs
        .iter()
        .map(|org| org.slug.as_str())
        .collect::<Vec<_>>();
    let next_command = login_next_command(&org_slugs);
    let replacing_existing_token = token_store
        .load(&account)
        .with_context(|| format!("failed to read {} store", token_store.kind()))?
        .is_some();
    token_store
        .save(&account, &token)
        .with_context(|| format!("failed to save token to {} store", token_store.kind()))?;

    if ctx.json {
        let payload = LoginJson {
            server: &url,
            store: token_store.kind(),
            env_override_present: env_token_present(),
            replaced_existing_token: replacing_existing_token,
            user: &identity.user,
            email: &identity.email,
            name: identity.name.as_deref(),
            server_admin: identity.server_admin,
            orgs: &identity.orgs,
            auth_method: login.auth_method,
            next_command: next_command.filter(|command| !is_template_command(command)),
            next_command_template: next_command.filter(|command| is_template_command(command)),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say_always(format!("logged in to {url}"));
    if ctx.quiet {
        return Ok(());
    }

    ctx.say(format!("  email: {}", identity.email));
    if let Some(name) = &identity.name {
        ctx.say(format!("  name:  {name}"));
    }
    if org_slugs.is_empty() {
        ctx.say("orgs: (none)");
    } else {
        ctx.say(format!("orgs: {}", org_slugs.join(", ")));
    }
    if let Some(command) = next_command {
        ctx.say(format!("next: {command}"));
    }
    ctx.say(format!("  token: stored in {}", token_store.kind()));
    ctx.say(format!("  auth:  {}", login.auth_method));
    if replacing_existing_token {
        ctx.say("  token: replaced existing stored token");
    }
    if env_token_present() {
        ctx.say(
            "  note: AGENTSTACK_TOKEN is set in this environment and will override the stored token",
        );
    }

    Ok(())
}

fn login_next_command(org_slugs: &[&str]) -> Option<&'static str> {
    match org_slugs {
        [_slug] => Some("agentstack skill list"),
        [] => None,
        _ => Some("agentstack skill list --org <org>"),
    }
}

fn is_template_command(command: &str) -> bool {
    command.contains('<') || command.contains('>')
}

struct LoginToken {
    token: Token,
    auth_method: &'static str,
}

fn read_or_request_token(ctx: &Ctx, url: &str, args: &Args) -> Result<LoginToken> {
    let force_oauth = args.no_browser || args.callback_port.is_some();
    if args.token_stdin || (!force_oauth && !io::stdin().is_terminal()) {
        let raw_token = read_token(ctx, args.token_stdin)?;
        return Ok(LoginToken {
            token: Token::new(raw_token),
            auth_method: "token_stdin",
        });
    }

    if ctx.no_input() {
        return Err(oauth_interactive_required_error().into());
    }

    let token = run_oauth_login(ctx, url, args)?;
    Ok(LoginToken {
        token,
        auth_method: "oauth_browser",
    })
}

fn run_oauth_login(ctx: &Ctx, url: &str, args: &Args) -> Result<Token> {
    let listener = bind_callback_listener(args.callback_port)?;
    let local_addr = listener
        .local_addr()
        .context("failed to inspect OAuth callback listener")?;
    let redirect_uri = format!("http://127.0.0.1:{}/auth/callback", local_addr.port());
    let code_verifier = random_urlsafe(32);
    let code_challenge = pkce_s256_challenge(&code_verifier);
    let state = random_urlsafe(32);

    let client = HttpRegistryClient::new(RegistryConnection::new(url.to_string(), None));
    let start = client.oauth_start(&OAuthStartRequest {
        provider: args.provider.clone(),
        redirect_uri: redirect_uri.clone(),
        code_challenge,
        code_challenge_method: "S256".to_string(),
        state: state.clone(),
        client: "agentstack-cli".to_string(),
        cli_version: env!("CARGO_PKG_VERSION").to_string(),
    })?;
    validate_authorization_url(url, &start.authorization_url)?;
    if let Some(returned_state) = start.state.as_deref()
        && returned_state != state
    {
        return Err(CliError::new(
            "oauth_state_mismatch",
            "registry returned OAuth state that did not match the login request",
        )
        .action("auth login")
        .status("state_mismatch")
        .next_command("agentstack auth login")
        .into());
    }
    let expected_state = state;

    if !ctx.quiet {
        ctx.say(format!("starting {} OAuth login for {url}", args.provider));
    }
    if args.no_browser {
        ctx.say_always("open this URL to continue:");
        ctx.say_always(&start.authorization_url);
    } else if let Err(err) = webbrowser::open(&start.authorization_url) {
        ctx.warn(format!(
            "warning: failed to open browser automatically: {err}; open this URL to continue:"
        ));
        ctx.say_always(&start.authorization_url);
    } else if !ctx.quiet {
        ctx.say(opened_browser_message());
    }

    let callback = wait_for_callback(
        listener,
        Duration::from_secs(args.timeout_seconds),
        &expected_state,
    )?;
    if callback.state != expected_state {
        return Err(CliError::new(
            "oauth_state_mismatch",
            "OAuth callback state did not match the login request",
        )
        .action("auth login")
        .status("state_mismatch")
        .next_command("agentstack auth login")
        .into());
    }
    if let Some(error) = callback.error {
        return Err(CliError::new(
            "oauth_denied",
            format!(
                "OAuth login failed: {}",
                sanitized_oauth_callback_error(&error)
            ),
        )
        .action("auth login")
        .status("provider_error")
        .next_command("agentstack auth login")
        .into());
    }
    let Some(code) = callback.code else {
        return Err(CliError::new(
            "oauth_missing_code",
            "OAuth callback did not include a code",
        )
        .action("auth login")
        .status("missing_code")
        .next_command("agentstack auth login")
        .into());
    };

    let exchanged = client.oauth_exchange(&OAuthExchangeRequest {
        grant_type: "authorization_code".to_string(),
        provider: args.provider.clone(),
        code,
        state: expected_state,
        redirect_uri,
        code_verifier,
    })?;
    if !exchanged.token_type.eq_ignore_ascii_case("bearer") {
        bail!("OAuth token response used unsupported token_type");
    }
    Token::from_material("OAuth exchange token", &exchanged.access_token)
}

fn validate_login_token(url: &str, token: Token) -> Result<WhoamiResponse> {
    let client = HttpRegistryClient::new(RegistryConnection::new(url.to_string(), Some(token)));
    match client.whoami() {
        Ok(identity) => Ok(identity),
        Err(err) => Err(sanitized_login_validation_error(url, &err).into()),
    }
}

fn sanitized_login_validation_error(url: &str, err: &anyhow::Error) -> CliError {
    let registry_error = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<CliError>());
    let http_status = registry_error.and_then(|error| error.http_status);
    let status = http_status
        .map(|status| format!(" with HTTP {status}"))
        .unwrap_or_default();
    let next_command = registry_error
        .and_then(|error| error.next_command.clone())
        .unwrap_or_else(|| "agentstack registry ping --auth".to_string());
    let code = registry_error
        .map(|error| error.code.as_str())
        .filter(|code| *code == "unauthenticated")
        .unwrap_or("login_validation_failed");

    let mut error = CliError::new(
        code,
        format!("login validation against {url} failed{status}"),
    )
    .resource(url)
    .action("auth login")
    .next_command(next_command);
    if code == "unauthenticated" {
        error = error
            .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
            .auth_methods(["auth_login", "AGENTSTACK_TOKEN_PATH", "AGENTSTACK_TOKEN"]);
    }
    if let Some(http_status) = http_status {
        error = error.http_status(http_status);
    }
    error
}

fn read_token(ctx: &Ctx, token_stdin: bool) -> Result<String> {
    if should_refuse_token_stdin(ctx, io::stdin().is_terminal(), token_stdin) {
        return Err(missing_token_error().into());
    }
    let (token, source) = match token_stdin {
        true => (read_stdin_token()?, TokenInputSource::ExplicitStdin),
        false if !io::stdin().is_terminal() => {
            let token = read_stdin_token()?;
            if ctx.no_input() && token.trim_ascii().is_empty() {
                return Err(missing_token_error().into());
            }
            (token, TokenInputSource::ImplicitStdin)
        }
        false if ctx.no_input() => {
            return Err(missing_token_error().into());
        }
        false => (
            rpassword::prompt_password("AgentStack token: ")
                .context("failed to read token from terminal")?,
            TokenInputSource::Prompt,
        ),
    };

    normalize_token_material(token_input_label(source), &token)
}

fn should_refuse_token_stdin(ctx: &Ctx, stdin_is_terminal: bool, token_stdin: bool) -> bool {
    token_stdin && ctx.no_input() && stdin_is_terminal
}

fn missing_token_message() -> &'static str {
    "token required; humans run `agentstack auth login` in a terminal for browser OAuth or pipe a token with --token-stdin; agents and CI set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN"
}

fn missing_token_error() -> CliError {
    CliError::new("token_required", missing_token_message())
        .action("auth login")
        .status("missing_token")
        .next_command("agentstack auth login")
        .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
        .auth_methods([
            "oauth_browser",
            "auth_login_token_stdin",
            "AGENTSTACK_TOKEN_PATH",
            "AGENTSTACK_TOKEN",
        ])
}

fn oauth_interactive_required_error() -> CliError {
    CliError::new(
        "oauth_interactive_required",
        "browser OAuth login requires an interactive terminal; agents and CI should set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN",
    )
    .action("auth login")
    .status("interactive_required")
    .next_command("agentstack auth login")
    .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
    .auth_methods([
        "oauth_browser",
        "auth_login_token_stdin",
        "AGENTSTACK_TOKEN_PATH",
        "AGENTSTACK_TOKEN",
    ])
}

fn read_stdin_token() -> Result<String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .context("failed to read token from stdin")?;
    Ok(input)
}

#[derive(Debug, Clone, Copy)]
enum TokenInputSource {
    ExplicitStdin,
    ImplicitStdin,
    Prompt,
}

fn token_input_label(source: TokenInputSource) -> &'static str {
    match source {
        TokenInputSource::ExplicitStdin => "--token-stdin input",
        TokenInputSource::ImplicitStdin => "stdin token input",
        TokenInputSource::Prompt => "token input",
    }
}

fn bind_callback_listener(port: Option<u16>) -> Result<TcpListener> {
    let addr = ("127.0.0.1", callback_port_or_default(port));
    let listener =
        TcpListener::bind(addr).with_context(|| "failed to bind OAuth loopback callback")?;
    listener
        .set_nonblocking(true)
        .context("failed to configure OAuth callback listener")?;
    Ok(listener)
}

fn callback_port_or_default(port: Option<u16>) -> u16 {
    port.unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT)
}

fn wait_for_callback(
    listener: TcpListener,
    timeout: Duration,
    expected_state: &str,
) -> Result<OAuthCallback> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .context("failed to configure OAuth callback stream")?;
                return handle_callback_stream(&mut stream, expected_state);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(CliError::new(
                        "oauth_timeout",
                        "timed out waiting for OAuth browser callback",
                    )
                    .action("auth login")
                    .status("timeout")
                    .next_command("agentstack auth login")
                    .into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err).context("failed while waiting for OAuth callback"),
        }
    }
}

fn handle_callback_stream(stream: &mut TcpStream, expected_state: &str) -> Result<OAuthCallback> {
    let mut buf = [0_u8; 8192];
    let n = stream
        .read(&mut buf)
        .context("failed to read OAuth callback")?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let callback = parse_callback_request(&request);
    let response = match &callback {
        Ok(callback)
            if callback.error.is_none()
                && callback.state == expected_state
                && callback.code.is_some() =>
        {
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\n\r\n<html><body><h1>You are signed in to AgentStack</h1><p>You can close this tab and return to your terminal.</p></body></html>"
        }
        Ok(_) | Err(_) => {
            "HTTP/1.1 400 Bad Request\r\ncontent-type: text/html; charset=utf-8\r\nconnection: close\r\n\r\n<html><body><h1>AgentStack login failed</h1><p>You can return to your terminal.</p></body></html>"
        }
    };
    let _ = stream.write_all(response.as_bytes());
    callback
}

fn sanitized_oauth_callback_error(error: &str) -> String {
    let trimmed = error.trim();
    if trimmed.is_empty()
        || trimmed.len() > 64
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return "provider_error".to_string();
    }
    trimmed.to_string()
}

fn opened_browser_message() -> &'static str {
    "opened browser for AgentStack login"
}

fn parse_callback_request(request: &str) -> Result<OAuthCallback> {
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_ascii_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if method != "GET" || target.is_empty() {
        bail!("OAuth callback must be an HTTP GET request");
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("OAuth callback URL was invalid")?;
    if url.path() != "/auth/callback" {
        bail!("OAuth callback path was invalid");
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }
    let state = state.ok_or_else(|| anyhow::anyhow!("OAuth callback did not include state"))?;
    let _ = error_description;
    Ok(OAuthCallback { code, state, error })
}

struct OAuthCallback {
    code: Option<String>,
    state: String,
    error: Option<String>,
}

fn random_urlsafe(bytes_len: usize) -> String {
    let mut bytes = vec![0_u8; bytes_len];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_s256_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn validate_authorization_url(registry_url: &str, value: &str) -> Result<()> {
    let registry =
        Url::parse(registry_url).context("active registry URL was invalid during OAuth login")?;
    let url = Url::parse(value).context("registry returned an invalid OAuth authorization URL")?;
    let same_registry_origin = url.scheme() == registry.scheme()
        && url.host_str() == registry.host_str()
        && url.port_or_known_default() == registry.port_or_known_default()
        && url.username().is_empty()
        && url.password().is_none();
    let google_origin = url.scheme() == "https"
        && url.host_str() == Some("accounts.google.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none();
    if !same_registry_origin && !google_origin {
        bail!(
            "registry returned an OAuth authorization URL outside the expected registry or provider origin"
        );
    }
    Ok(())
}

#[derive(Serialize)]
struct LoginJson<'a> {
    server: &'a str,
    store: &'a str,
    env_override_present: bool,
    replaced_existing_token: bool,
    user: &'a str,
    email: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    server_admin: bool,
    orgs: &'a [OrgMembership],
    auth_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_stdin_refuses_noninteractive_tty_to_avoid_hanging() {
        let ctx = Ctx {
            no_input: true,
            ..Ctx::default()
        };

        assert!(should_refuse_token_stdin(&ctx, true, true));
        assert!(!should_refuse_token_stdin(&ctx, false, true));
        assert!(!should_refuse_token_stdin(&ctx, true, false));
    }

    #[test]
    fn callback_port_defaults_to_registered_loopback_port() {
        assert_eq!(callback_port_or_default(None), DEFAULT_OAUTH_CALLBACK_PORT);
        assert_eq!(callback_port_or_default(Some(0)), 0);
    }

    #[test]
    fn authorization_url_accepts_registry_origin_and_google_only() {
        validate_authorization_url(
            "https://registry.agentstack.gg",
            "https://registry.agentstack.gg/v1/auth/oauth/google/authorize?flow=test",
        )
        .unwrap();
        validate_authorization_url(
            "https://registry.agentstack.gg",
            "https://accounts.google.com/o/oauth2/v2/auth?flow=test",
        )
        .unwrap();

        assert!(
            validate_authorization_url(
                "https://registry.agentstack.gg",
                "https://user@registry.agentstack.gg/v1/auth/oauth/google/authorize",
            )
            .is_err()
        );
        assert!(
            validate_authorization_url(
                "https://registry.agentstack.gg",
                "https://evil.example.com"
            )
            .is_err()
        );
    }

    #[test]
    fn browser_opened_copy_stays_quiet() {
        let message = opened_browser_message();
        assert_eq!(message, "opened browser for AgentStack login");
        assert!(!message.contains("Opened"));
        assert!(!message.ends_with('.'));
    }
}
