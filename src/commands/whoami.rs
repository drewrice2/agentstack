use anyhow::{Context, Result};
use serde::Serialize;

use super::client::validate_resolved_registry_url;
use crate::config::ConfigStore;
use crate::credentials::{
    DEFAULT_ACCOUNT, Token, TokenSource, resolve_token, scoped_account, token_store,
};
use crate::error::CliError;
use crate::output::Ctx;
use crate::registry::{
    HttpRegistryClient, OrgMembership, RegistryClient, RegistryConnection, WhoamiResponse,
};

pub struct Args {
    pub local: bool,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let cfg = ConfigStore::load().context("failed to load config")?;
    let store = token_store();
    let resolved_url = cfg.resolved_registry_url();
    validate_resolved_registry_url(&resolved_url)?;
    let url = resolved_url.url;
    let account = scoped_account(&url, DEFAULT_ACCOUNT)?;
    let resolved = resolve_token(store.as_ref(), &account)?;

    if args.local {
        return render_local(ctx, &url, store.kind(), resolved.as_ref());
    }

    let Some((token, source)) = resolved else {
        return Err(not_logged_in_error(&url).into());
    };

    let client = HttpRegistryClient::new(RegistryConnection::new(url.clone(), Some(token)));
    let reply = client
        .whoami()
        .with_context(|| format!("whoami on {url} failed"))?;

    render_remote(ctx, &url, &reply, source, store.kind())
}

fn render_local(
    ctx: &Ctx,
    server: &str,
    store_kind: &str,
    resolved: Option<&(Token, TokenSource)>,
) -> Result<()> {
    let source = resolved.map(|(_, source)| source_label(*source, store_kind));
    if ctx.json {
        let payload = WhoamiLocalJson {
            logged_in: resolved.is_some(),
            server,
            token_present: resolved.is_some(),
            source,
            store: store_kind,
            next_command: local_next_command(resolved.is_some()),
            next_command_template: None,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say("local auth status");
    ctx.say(format!("  server: {server}"));
    ctx.say(format!(
        "  token:  {}",
        if resolved.is_some() {
            "present"
        } else {
            "not found"
        }
    ));
    if let Some(source) = source {
        ctx.say(format!("  source: {source}"));
    }
    ctx.say(format!("  store:  {store_kind}"));
    if let Some(command) = local_next_command(resolved.is_some()) {
        ctx.say(format!("next: {command}"));
    }
    Ok(())
}

fn not_logged_in_error(server: &str) -> CliError {
    CliError::new(
        "unauthenticated",
        format!(
            "not logged in; humans run `{LOGIN_NEXT_COMMAND}`; agents and CI set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN"
        ),
    )
    .action("authenticate")
    .status("not_logged_in")
    .next_command(LOGIN_NEXT_COMMAND)
    .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
    .auth_methods(["auth_login", "AGENTSTACK_TOKEN_PATH", "AGENTSTACK_TOKEN"])
    .resource(server)
}

fn render_remote(
    ctx: &Ctx,
    server: &str,
    reply: &WhoamiResponse,
    source: TokenSource,
    store_kind: &str,
) -> Result<()> {
    let source = source_label(source, store_kind);
    if ctx.json {
        let payload = WhoamiRemoteJson {
            logged_in: true,
            server,
            user: &reply.user,
            org: reply.org.as_deref(),
            email: &reply.email,
            name: reply.name.as_deref(),
            server_admin: reply.server_admin,
            orgs: &reply.orgs,
            source,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say("logged in");
    ctx.say(format!("  server:       {server}"));
    ctx.say(format!("  email:        {}", reply.email));
    if let Some(name) = &reply.name {
        ctx.say(format!("  name:         {name}"));
    }
    ctx.say(format!("  server_admin: {}", reply.server_admin));
    ctx.say(format!("  source:       {source}"));
    if reply.orgs.is_empty() {
        ctx.say("  orgs:         (none)");
    } else {
        ctx.say("  orgs:");
        for org in &reply.orgs {
            ctx.say(format!(
                "    - {} ({}) role={}",
                org.slug, org.name, org.role
            ));
        }
    }
    Ok(())
}

fn source_label(source: TokenSource, store_kind: &str) -> String {
    match source {
        TokenSource::Env => "AGENTSTACK_TOKEN".to_string(),
        TokenSource::Path => "AGENTSTACK_TOKEN_PATH".to_string(),
        TokenSource::Store => store_kind.to_string(),
    }
}

const LOGIN_NEXT_COMMAND: &str = "agentstack auth login";

fn local_next_command(token_present: bool) -> Option<&'static str> {
    (!token_present).then_some(LOGIN_NEXT_COMMAND)
}

#[derive(Serialize)]
struct WhoamiLocalJson<'a> {
    logged_in: bool,
    server: &'a str,
    token_present: bool,
    source: Option<String>,
    store: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<&'static str>,
}

#[derive(Serialize)]
struct WhoamiRemoteJson<'a> {
    logged_in: bool,
    server: &'a str,
    user: &'a str,
    org: Option<&'a str>,
    email: &'a str,
    name: Option<&'a str>,
    server_admin: bool,
    orgs: &'a [OrgMembership],
    source: String,
}
