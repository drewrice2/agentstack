use anyhow::{Context, Result};
use serde::Serialize;

use crate::cache::{self, Cache, CacheEntry};
use crate::cli::CacheCommand;
use crate::output::Ctx;

pub fn run(ctx: &Ctx, action: CacheCommand) -> Result<()> {
    let cache = Cache::from_config().context("failed to open cache")?;
    match action {
        CacheCommand::Path => {
            ctx.say_always(format!("{}", cache.root().display()));
            Ok(())
        }
        CacheCommand::List => list(ctx, &cache),
        CacheCommand::Remove { name, force } => remove(ctx, &cache, &name, force),
    }
}

fn list(ctx: &Ctx, cache: &Cache) -> Result<()> {
    let entries = cache.list().context("failed to read cache")?;

    if ctx.json {
        println!("{}", render_json(cache, &entries)?);
        return Ok(());
    }

    if entries.is_empty() {
        ctx.say(cache::empty_message(cache));
        ctx.say("next: agentstack skill pack ./my-skill");
        return Ok(());
    }
    let name_w = entries
        .iter()
        .map(|e| e.manifest.name.len())
        .max()
        .unwrap_or(0);
    let ver_w = entries
        .iter()
        .map(|e| e.manifest.version.len())
        .max()
        .unwrap_or(0);
    for entry in &entries {
        println!(
            "{name:<name_w$}  {ver:<ver_w$}  {hash}  {size:>9}",
            name = entry.manifest.name,
            ver = entry.manifest.version,
            hash = entry.hash.short(),
            size = cache::human_bytes(entry.size_bytes),
            name_w = name_w,
            ver_w = ver_w,
        );
    }
    Ok(())
}

fn remove(ctx: &Ctx, cache: &Cache, name: &str, force: bool) -> Result<()> {
    cache::check_name_arg(name)?;
    let exists = cache
        .list()
        .context("failed to read cache")?
        .iter()
        .any(|entry| entry.manifest.name == name);
    if !exists {
        return Err(anyhow::anyhow!(
            "no cached skill named `{name}` (cache root: {})",
            cache.root().display()
        ));
    }
    if !force && !confirm_remove(ctx, name)? {
        ctx.say(format!("aborted: `{name}` not removed"));
        return Ok(());
    }
    if cache.remove(name)? {
        if ctx.json {
            let skills_dir = cache.skills_dir();
            let payload = CacheRemoveJson {
                name,
                removed: true,
                root: cache.root(),
                skills_dir: &skills_dir,
            };
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }
        ctx.say(format!(
            "removed `{name}` from {}",
            cache.skills_dir().display()
        ));
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "no cached skill named `{name}` (cache root: {})",
            cache.root().display()
        ))
    }
}

/// Prompt the user before a destructive cache removal. In non-interactive
/// contexts, refuse without `--force`.
fn confirm_remove(ctx: &Ctx, name: &str) -> Result<bool> {
    ctx.prompt_confirm(
        format!("remove every cached version of `{name}`?"),
        format!("refusing to remove `{name}` non-interactively; rerun with `--force` to confirm"),
    )
}

#[derive(Serialize)]
struct CacheJson<'a> {
    root: &'a std::path::Path,
    entries: Vec<CacheEntryJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
}

#[derive(Serialize)]
struct CacheEntryJson<'a> {
    name: &'a str,
    version: &'a str,
    hash: &'a str,
    size_bytes: u64,
    package_path: &'a std::path::Path,
}

#[derive(Serialize)]
struct CacheRemoveJson<'a> {
    name: &'a str,
    removed: bool,
    root: &'a std::path::Path,
    skills_dir: &'a std::path::Path,
}

fn render_json(cache: &Cache, entries: &[CacheEntry]) -> Result<String> {
    let out = CacheJson {
        root: cache.root(),
        entries: entries
            .iter()
            .map(|e| CacheEntryJson {
                name: &e.manifest.name,
                version: &e.manifest.version,
                hash: &e.hash.hex,
                size_bytes: e.size_bytes,
                package_path: &e.package_path,
            })
            .collect(),
        empty_message: entries.is_empty().then(|| cache::empty_message(cache)),
        next_command: entries
            .is_empty()
            .then_some("agentstack skill pack ./my-skill"),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}
