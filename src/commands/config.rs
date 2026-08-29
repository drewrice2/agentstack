use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cli::ConfigCommand;
use crate::config::{self, ConfigStore};
use crate::output::Ctx;
use crate::targets::InstallTarget;

pub fn run(ctx: &Ctx, action: ConfigCommand) -> Result<()> {
    match action {
        ConfigCommand::Path => {
            ctx.say_always(format!("{}", config::config_dir()?.display()));
            Ok(())
        }
        ConfigCommand::Show => show(ctx),
    }
}

fn show(ctx: &Ctx) -> Result<()> {
    let store = ConfigStore::load().context("failed to load config")?;

    if ctx.json {
        let payload = ConfigShowJson {
            path: store.path(),
            config: store.config(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say_always(format!("config: {}", store.path().display()));

    if store.config().is_empty() {
        ctx.say("(empty — no overrides set)");
        ctx.say("");
        ctx.say("next:");
        ctx.say("  agentstack target set <target> --path <path>");
        ctx.say("  agentstack registry use <URL>");
        return Ok(());
    }

    if let Some(url) = store.registry_url() {
        println!();
        println!("[registry]");
        println!("  url = {url}");
    }

    if !store.config().targets.is_empty() {
        println!();
        println!("[targets]");
        let key_w = store
            .config()
            .targets
            .keys()
            .map(|k| k.len())
            .max()
            .unwrap_or(0);
        for (name, path) in &store.config().targets {
            println!("  {name:<key_w$} = {}", path.display(), key_w = key_w);
        }
    }
    Ok(())
}

pub(crate) fn set_target(ctx: &Ctx, target_name: &str, path: PathBuf) -> Result<()> {
    let target = InstallTarget::parse(target_name)?;
    if !path.is_absolute() {
        bail!(
            "target override path must be absolute (got `{}`)",
            path.display()
        );
    }
    let mut store = ConfigStore::load().context("failed to load config")?;
    store.set_target(target.as_str().to_string(), path.clone());
    store.save().context("failed to write config")?;
    if ctx.json {
        let payload = ConfigSetTargetJson {
            target: target.as_str(),
            path: &path,
            config: store.path(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    ctx.say(format!(
        "set target `{}` -> {}",
        target.as_str(),
        path.display()
    ));
    ctx.say(format!("({})", store.path().display()));
    Ok(())
}

pub(crate) fn unset_target(ctx: &Ctx, target_name: &str) -> Result<()> {
    let target = InstallTarget::parse(target_name)?;
    let mut store = ConfigStore::load().context("failed to load config")?;
    match store.unset_target(target.as_str()) {
        Some(prev) => {
            store.save().context("failed to write config")?;
            if ctx.json {
                let payload = ConfigUnsetTargetJson {
                    target: target.as_str(),
                    removed: true,
                    previous: Some(prev.as_path()),
                    config: store.path(),
                };
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            ctx.say(format!(
                "removed target `{}` (was {})",
                target.as_str(),
                prev.display()
            ));
        }
        None => {
            if ctx.json {
                let payload = ConfigUnsetTargetJson {
                    target: target.as_str(),
                    removed: false,
                    previous: None,
                    config: store.path(),
                };
                println!("{}", serde_json::to_string_pretty(&payload)?);
                return Ok(());
            }
            ctx.say(format!(
                "target `{}` was not set; nothing to do",
                target.as_str()
            ));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct ConfigShowJson<'a> {
    path: &'a std::path::Path,
    config: &'a crate::config::AgentStackConfig,
}

#[derive(Serialize)]
struct ConfigSetTargetJson<'a> {
    target: &'static str,
    path: &'a std::path::Path,
    config: &'a std::path::Path,
}

#[derive(Serialize)]
struct ConfigUnsetTargetJson<'a> {
    target: &'static str,
    removed: bool,
    previous: Option<&'a std::path::Path>,
    config: &'a std::path::Path,
}
