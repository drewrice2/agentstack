use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::ConfigStore;
use crate::output::Ctx;
use crate::targets::{InstallTarget, TargetDetection, TargetResolver};

fn load_store() -> Result<ConfigStore> {
    ConfigStore::load().context("failed to load config")
}

pub fn list(ctx: &Ctx) -> Result<()> {
    let store = load_store()?;
    let resolver = TargetResolver::new(&store);
    if ctx.json {
        return list_json(&resolver);
    }

    let rows: Vec<(InstallTarget, &'static str, String, String, &'static str)> = resolver
        .resolve_all()
        .into_iter()
        .map(|(t, res)| match res {
            Ok(r) => (
                t,
                t.alias().unwrap_or("-"),
                r.path.display().to_string(),
                r.source.as_str().into(),
                t.description(),
            ),
            Err(_) => (
                t,
                t.alias().unwrap_or("-"),
                "(no default — set with `agentstack target set`)".into(),
                "missing".into(),
                t.description(),
            ),
        })
        .collect();

    let name_w = rows
        .iter()
        .map(|(t, ..)| t.as_str().len())
        .max()
        .unwrap_or(0);
    let alias_w = rows
        .iter()
        .map(|(_, alias, ..)| alias.len())
        .max()
        .unwrap_or(0);
    let path_w = rows.iter().map(|(_, _, p, ..)| p.len()).max().unwrap_or(0);
    let src_w = rows.iter().map(|(.., src, _)| src.len()).max().unwrap_or(0);

    println!(
        "{name:<name_w$}  {alias:<alias_w$}  {path:<path_w$}  {src:<src_w$}  description",
        name = "target",
        alias = "alias",
        path = "path",
        src = "source",
        name_w = name_w,
        alias_w = alias_w,
        path_w = path_w,
        src_w = src_w,
    );
    println!(
        "{}",
        "-".repeat(name_w + alias_w + path_w + src_w + "description".len() + 8)
    );
    for (t, alias, p, src, desc) in &rows {
        println!(
            "{name:<name_w$}  {alias:<alias_w$}  {path:<path_w$}  {src:<src_w$}  {desc}",
            name = t.as_str(),
            alias = alias,
            path = p,
            src = src,
            desc = desc,
            name_w = name_w,
            alias_w = alias_w,
            path_w = path_w,
            src_w = src_w,
        );
    }
    ctx.say("");
    ctx.say("next:");
    ctx.say("  agentstack target set <target> --path <path>");
    Ok(())
}

pub fn path(ctx: &Ctx, target_name: &str) -> Result<()> {
    let store = load_store()?;
    let resolver = TargetResolver::new(&store);
    let target = InstallTarget::parse(target_name)?;
    let resolved = resolver.resolve(target)?;
    if ctx.json {
        let payload = TargetPathJson {
            target: resolved.target.as_str(),
            path: &resolved.path,
            source: resolved.source.as_str(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    ctx.say_always(format!("{}", resolved.path.display()));
    Ok(())
}

pub fn detect(ctx: &Ctx) -> Result<()> {
    let store = load_store()?;
    let resolver = TargetResolver::new(&store);
    let rows = resolver.detect_all();
    if ctx.json {
        let (next_commands, next_command_templates) = detect_next_guidance(&rows);
        let payload = TargetDetectJson {
            targets: &rows,
            next_commands,
            next_command_templates,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let name_w = rows.iter().map(|r| r.target.len()).max().unwrap_or(0);
    let path_w = rows
        .iter()
        .map(|r| {
            r.path
                .as_ref()
                .map(|p| p.display().to_string().len())
                .unwrap_or(9)
        })
        .max()
        .unwrap_or(0);
    println!(
        "{name:<name_w$}  {path:<path_w$}  configured  source   exists  writable  usable",
        name = "target",
        path = "path",
        name_w = name_w,
        path_w = path_w,
    );
    println!(
        "{}",
        "-".repeat(name_w + path_w + "configured  source   exists  writable  usable".len() + 4)
    );
    for row in &rows {
        let path = row
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<missing>".to_string());
        println!(
            "{name:<name_w$}  {path:<path_w$}  {configured:<10}  {source:<7}  {exists:<6}  {writable:<8}  {usable}",
            name = row.target,
            path = path,
            configured = yes_no(row.configured),
            source = row.source,
            exists = yes_no(row.exists),
            writable = yes_no(row.writable),
            usable = yes_no(row.usable),
            name_w = name_w,
            path_w = path_w,
        );
    }

    let fixes: Vec<&str> = rows
        .iter()
        .filter_map(|row| row.fix_command.as_deref())
        .collect();
    if !fixes.is_empty() {
        ctx.say("");
        ctx.say("next:");
        for command in fixes {
            ctx.say(format!("  {command}"));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct TargetRow {
    target: &'static str,
    path: Option<String>,
    source: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct TargetPathJson<'a> {
    target: &'static str,
    path: &'a std::path::Path,
    source: &'static str,
}

#[derive(Serialize)]
struct TargetsJson<'a> {
    targets: &'a [TargetRow],
}

#[derive(Serialize)]
struct TargetDetectJson<'a, T> {
    targets: &'a [T],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    next_commands: Vec<String>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    next_command_templates: Vec<String>,
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn detect_next_guidance(rows: &[TargetDetection]) -> (Vec<String>, Vec<String>) {
    let mut commands = Vec::new();
    let mut templates = Vec::new();
    for command in rows.iter().filter_map(|row| row.fix_command.clone()) {
        if crate::output::is_concrete_next_command(&command) {
            commands.push(command);
        } else {
            templates.push(command);
        }
    }
    (commands, templates)
}

fn list_json(resolver: &TargetResolver<'_>) -> Result<()> {
    let rows: Vec<TargetRow> = resolver
        .resolve_all()
        .into_iter()
        .map(|(t, res)| match res {
            Ok(r) => TargetRow {
                target: t.as_str(),
                path: Some(r.path.display().to_string()),
                source: r.source.as_str(),
                description: t.description(),
            },
            Err(_) => TargetRow {
                target: t.as_str(),
                path: None,
                source: "missing",
                description: t.description(),
            },
        })
        .collect();
    let payload = TargetsJson { targets: &rows };
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}
