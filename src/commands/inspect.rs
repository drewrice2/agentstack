use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::error::CliError;
use crate::output::Ctx;
use crate::skill::{DirectorySummary, LintConfig, SkillInspection, ValidationCode, inspect_skill};

pub fn run(ctx: &Ctx, path: Option<PathBuf>, max_chars: usize) -> Result<()> {
    let target = path.unwrap_or_else(|| PathBuf::from("."));
    let config = LintConfig {
        soft_char_limit: max_chars,
    };
    let inspection = inspect_skill(&target, &config);
    let target_missing = inspection
        .errors
        .iter()
        .any(|error| error.code == ValidationCode::NotADirectory);

    if ctx.json {
        if target_missing {
            return Err(inspect_target_error(&target).into());
        }
        let s = serde_json::to_string_pretty(&inspection)
            .context("failed to serialize inspection as JSON")?;
        println!("{s}");
        return Ok(());
    }

    print_human(ctx, &inspection);
    if target_missing {
        return Err(inspect_target_error(&target).into());
    }
    Ok(())
}

fn inspect_target_error(target: &std::path::Path) -> CliError {
    CliError::new(
        "not_a_directory",
        format!("`{}` is not a skill directory", target.display()),
    )
    .resource(target.display().to_string())
    .action("inspect_skill")
    .next_command("agentstack skill scan")
}

fn print_human(ctx: &Ctx, insp: &SkillInspection) {
    if let Some(name) = &insp.name {
        ctx.say(format!("name:        {name}"));
    } else {
        ctx.say("name:        <missing>");
    }
    if let Some(description) = &insp.description {
        ctx.say(format!("description: {description}"));
    } else {
        ctx.say("description: <missing>");
    }
    ctx.say(format!("path:        {}", insp.path.display()));
    if let Some(md) = &insp.skill_md {
        ctx.say(format!("SKILL.md:    {} characters", md.char_count));
        ctx.say("sections:");
        if md.sections.is_empty() {
            ctx.say("  (none)");
        }
        for s in &md.sections {
            ctx.say(format!("  - {s}"));
        }
    } else {
        ctx.say("SKILL.md:    <not parsed>");
    }

    ctx.say("directories:");
    print_dir(ctx, "references", &insp.directories.references);
    print_dir(ctx, "examples", &insp.directories.examples);
    print_dir(ctx, "assets", &insp.directories.assets);
    print_dir(ctx, "platform", &insp.directories.platform);

    if !insp.unknown_files.is_empty() {
        ctx.say("unknown files:");
        for f in &insp.unknown_files {
            ctx.say(format!("  - {f}"));
        }
    }

    ctx.say(format!(
        "package hash: {}",
        insp.package_hash.as_deref().unwrap_or("(not computed)")
    ));

    if !insp.errors.is_empty() {
        ctx.say("errors:");
        for e in &insp.errors {
            ctx.say(e.to_string());
        }
    }
    if !insp.warnings.is_empty() {
        ctx.say("warnings:");
        for w in &insp.warnings {
            ctx.say(format!("  - [{}] {}", w.code, w.message));
        }
    }
}

fn print_dir(ctx: &Ctx, name: &str, summary: &DirectorySummary) {
    let count = summary.files.len();
    ctx.say(format!(
        "  {name:<11} {} ({count} file{})",
        if summary.present {
            "[present]"
        } else {
            "[missing]"
        },
        if count == 1 { "" } else { "s" }
    ));
    for f in &summary.files {
        ctx.say(format!("    - {f}"));
    }
}
