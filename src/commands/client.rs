//! Glue: pull a [`RegistryClient`] (and the URL we will print) out of the
//! active registry URL + credentials store. Centralised here so every remote
//! subcommand resolves the active registry URL identically.

use anyhow::{Context, Result};

use crate::config::{ConfigStore, REGISTRY_URL_ENV, RegistryUrlSource, ResolvedRegistryUrl};
use crate::credentials::{DEFAULT_ACCOUNT, resolve_token, scoped_account, token_store};
use crate::error::CliError;
use crate::output::Ctx;
use crate::registry::{
    HttpRegistryClient, RegistryClient, RegistryConnection, validate_registry_url,
};

pub struct ConfiguredClient {
    pub url: String,
    pub client: HttpRegistryClient,
}

pub fn configured_registry_url(cfg: &ConfigStore) -> Result<ResolvedRegistryUrl> {
    let resolved = cfg.resolved_registry_url();
    validate_resolved_registry_url(&resolved)?;
    Ok(resolved)
}

pub fn validate_resolved_registry_url(resolved: &ResolvedRegistryUrl) -> Result<()> {
    validate_registry_url(&resolved.url).with_context(|| match resolved.source {
        RegistryUrlSource::Env => format!("{REGISTRY_URL_ENV} is invalid"),
        RegistryUrlSource::Config => "persisted registry URL is invalid".to_string(),
        RegistryUrlSource::Default => "default registry URL is invalid".to_string(),
    })
}

/// Resolve the registry URL + token and build an [`HttpRegistryClient`].
///
/// Returns the URL alongside the client so the caller can quote it in error
/// messages without re-reading config.
pub fn configured_client() -> Result<ConfiguredClient> {
    let cfg = ConfigStore::load().context("failed to load config")?;
    let url = configured_registry_url(&cfg)?.url;

    let store = token_store();
    let account = scoped_account(&url, DEFAULT_ACCOUNT)?;
    let token = resolve_token(store.as_ref(), &account)?
        .map(|(t, _src)| t)
        .ok_or_else(|| {
            CliError::new(
                "unauthenticated",
                "not logged in; humans run `agentstack auth login`; agents and CI set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN",
            )
            .action("authenticate")
            .next_command("agentstack auth login")
            .machine_hint("set AGENTSTACK_TOKEN_PATH or AGENTSTACK_TOKEN for automation")
            .auth_methods(["auth_login", "AGENTSTACK_TOKEN_PATH", "AGENTSTACK_TOKEN"])
        })?;

    let client = HttpRegistryClient::new(RegistryConnection::new(url.clone(), Some(token)));
    Ok(ConfiguredClient { url, client })
}

/// Context line for a failed registry call: `"<with_url> <url> failed"` when
/// a registry URL is known, `"<without_url> failed"` otherwise.
pub fn registry_context(registry_url: Option<&str>, with_url: &str, without_url: &str) -> String {
    match registry_url {
        Some(url) => format!("{with_url} {url} failed"),
        None => format!("{without_url} failed"),
    }
}

/// [`configured_client`] plus the registry identity used as `installed_by`:
/// logs the registry URL in verbose mode and resolves the current user via
/// `whoami`, treating failures as anonymous.
pub fn configured_client_with_identity(ctx: &Ctx) -> Result<(ConfiguredClient, Option<String>)> {
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let installed_by = configured.client.whoami().ok().map(|reply| reply.user);
    Ok((configured, installed_by))
}
