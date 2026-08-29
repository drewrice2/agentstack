use anyhow::{Context, Result};
use serde::Serialize;

use super::client::configured_registry_url;
use crate::cli::RegistryCommand;
use crate::config::{ConfigStore, REGISTRY_URL_ENV, RegistryUrlSource};
use crate::credentials::{DEFAULT_ACCOUNT, resolve_token, scoped_account, token_store};
use crate::error::CliError;
use crate::output::Ctx;
use crate::registry::{
    HttpRegistryClient, RegistryClient, RegistryConnection, validate_registry_url,
};

pub fn run(ctx: &Ctx, action: RegistryCommand) -> Result<()> {
    match action {
        RegistryCommand::Ping { auth } => ping(ctx, auth),
        RegistryCommand::Use { url } => set(ctx, url),
        RegistryCommand::Show => get(ctx),
    }
}

fn set(ctx: &Ctx, url: String) -> Result<()> {
    validate_registry_url(&url)?;
    let mut store = ConfigStore::load().context("failed to load config")?;
    store.set_registry_url(url.clone());
    store.save().context("failed to write config")?;
    let env_override_present = registry_url_env_present();
    if ctx.json {
        let payload = RegistrySetJson {
            url: &url,
            config: store.path(),
            active_source: if env_override_present {
                REGISTRY_URL_ENV
            } else {
                "config"
            },
            saved_url_active: !env_override_present,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    ctx.say(format!("registry set to {url}"));
    ctx.say(format!("({})", store.path().display()));
    if env_override_present {
        ctx.say(format!(
            "note: saved to config, but {REGISTRY_URL_ENV} is currently active"
        ));
    }
    Ok(())
}

fn registry_url_env_present() -> bool {
    std::env::var_os(REGISTRY_URL_ENV).is_some_and(|value| !value.as_os_str().is_empty())
}

fn get(ctx: &Ctx) -> Result<()> {
    let store = ConfigStore::load().context("failed to load config")?;
    let resolved = configured_registry_url(&store)?;
    if ctx.json {
        let payload = RegistryGetJson {
            url: &resolved.url,
            source: resolved.source.label(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    match resolved.source {
        RegistryUrlSource::Env => {
            ctx.say_always(&resolved.url);
            ctx.say(format!("(from {})", resolved.source.label()));
        }
        RegistryUrlSource::Config => ctx.say_always(&resolved.url),
        RegistryUrlSource::Default => ctx.say_always(format!("{} (default)", resolved.url)),
    }
    Ok(())
}

fn ping(ctx: &Ctx, auth: bool) -> Result<()> {
    let cfg = ConfigStore::load().context("failed to load config")?;
    let url = configured_registry_url(&cfg)?.url;

    let token = if auth {
        let store = token_store();
        let account = scoped_account(&url, DEFAULT_ACCOUNT)?;
        resolve_token(store.as_ref(), &account)?.map(|(t, _src)| t)
    } else {
        None
    };

    let client = HttpRegistryClient::new(RegistryConnection::new(url.clone(), token));
    let reply = client.ping().map_err(|err| {
        CliError::new(
            "registry_unavailable",
            format!("could not reach {url}: {err}"),
        )
        .action("registry_request")
        .next_command("agentstack doctor")
    })?;
    let identity = if auth {
        Some(client.whoami().map_err(|err| {
            CliError::new(
                "registry_unavailable",
                format!("could not validate token with {url}: {err}"),
            )
            .action("registry_request")
            .next_command("agentstack doctor")
        })?)
    } else {
        None
    };

    if ctx.json {
        let payload = RegistryPingJson {
            url: &url,
            ok: true,
            // `null` when `--auth` was not passed (token was never checked),
            // distinct from `false` which would imply a failed check.
            authenticated: auth.then(|| identity.is_some()),
            email: identity.as_ref().map(|whoami| whoami.email.as_str()),
            server_version: &reply.server_version,
            next_command: (!auth).then_some("agentstack registry ping --auth"),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    ctx.say(format!("ok: {url}"));
    ctx.say(format!("  server version: {}", reply.server_version));
    if let Some(identity) = identity {
        ctx.say(format!("  auth: ok ({})", identity.email));
    } else {
        ctx.say("  auth: not checked (run `agentstack registry ping --auth`)");
    }
    Ok(())
}

#[derive(Serialize)]
struct RegistrySetJson<'a> {
    url: &'a str,
    config: &'a std::path::Path,
    active_source: &'a str,
    saved_url_active: bool,
}

#[derive(Serialize)]
struct RegistryGetJson<'a> {
    url: &'a str,
    source: &'a str,
}

#[derive(Serialize)]
struct RegistryPingJson<'a> {
    url: &'a str,
    ok: bool,
    authenticated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
    server_version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
}
