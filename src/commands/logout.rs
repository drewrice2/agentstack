use anyhow::{Context, Result};
use serde::Serialize;

use super::client::configured_registry_url;
use crate::config::ConfigStore;
use crate::credentials::{DEFAULT_ACCOUNT, env_token_present, scoped_account, token_store};
use crate::output::Ctx;

pub fn run(ctx: &Ctx) -> Result<()> {
    let cfg = ConfigStore::load().context("failed to load config")?;
    let url = configured_registry_url(&cfg)?.url;
    let account = scoped_account(&url, DEFAULT_ACCOUNT)?;
    let store = token_store();
    let had_token = store
        .load(&account)
        .with_context(|| format!("failed to read {} store", store.kind()))?
        .is_some();

    store
        .delete(&account)
        .with_context(|| format!("failed to delete token from {} store", store.kind()))?;

    if ctx.json {
        let payload = LogoutJson {
            removed: had_token,
            store: store.kind(),
            env_override_present: env_token_present(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if had_token {
        ctx.say(format!("removed stored token ({})", store.kind()));
    } else {
        ctx.say(format!("no stored token to remove ({})", store.kind()));
    }

    if env_token_present() {
        ctx.say(
            "note: AGENTSTACK_TOKEN is still set in this environment; unset it to fully log out",
        );
    }

    Ok(())
}

#[derive(Serialize)]
struct LogoutJson<'a> {
    removed: bool,
    store: &'a str,
    env_override_present: bool,
}
