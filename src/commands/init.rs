use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::output::Ctx;
use crate::skill::{
    SKILL_MD, STANDARD_SUBDIRS, SkillManifest, check_description, check_slug, render_skill_md,
};

pub struct Args {
    pub path: Option<PathBuf>,
    pub name: String,
    pub description: String,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if let Err(reason) = check_slug(&args.name) {
        bail!("invalid --name `{}`: {reason}", args.name);
    }
    if let Err(reason) = check_description(&args.description) {
        bail!("invalid --description: {reason}");
    }

    let target = args
        .path
        .clone()
        .unwrap_or_else(|| PathBuf::from(&args.name));

    if target.exists() {
        let metadata = fs::metadata(&target)
            .with_context(|| format!("failed to inspect `{}`", target.display()))?;
        if !metadata.is_dir() {
            bail!("`{}` exists and is not a directory", target.display());
        }
        let mut entries = fs::read_dir(&target)
            .with_context(|| format!("failed to read `{}`", target.display()))?;
        if entries.next().is_some() {
            bail!(
                "refusing to scaffold into non-empty directory: `{}`",
                target.display()
            );
        }
    } else {
        fs::create_dir_all(&target)
            .with_context(|| format!("failed to create `{}`", target.display()))?;
    }

    for sub in STANDARD_SUBDIRS {
        let dir = target.join(sub);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create `{}`", dir.display()))?;
        let keep = dir.join(".gitkeep");
        fs::write(&keep, b"").with_context(|| format!("failed to write `{}`", keep.display()))?;
    }

    let manifest = SkillManifest {
        name: args.name.clone(),
        description: args.description.clone(),
    };
    let skill_md = render_skill_md(&manifest)?;
    let skill_path = target.join(SKILL_MD);
    fs::write(&skill_path, skill_md)
        .with_context(|| format!("failed to write `{}`", skill_path.display()))?;

    if ctx.json {
        let payload = InitJson {
            name: &args.name,
            path: &target,
            skill_md: &skill_path,
            subdirs: STANDARD_SUBDIRS,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!("created skill `{}`", args.name,));
    ctx.say(format!("path: {}", target.display()));
    ctx.say(format!("skill.md: {}", skill_path.display()));
    ctx.say("");
    ctx.say("next:");
    ctx.say(format!("  agentstack skill validate {}", target.display()));
    ctx.say(format!("  agentstack skill lint {}", target.display()));
    Ok(())
}

#[derive(Serialize)]
struct InitJson<'a> {
    name: &'a str,
    path: &'a Path,
    skill_md: &'a Path,
    subdirs: &'a [&'a str],
}
