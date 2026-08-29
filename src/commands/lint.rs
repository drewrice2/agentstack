use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::output::Ctx;
use crate::skill::{LintConfig, LintWarning, ValidationError, lint_skill, validate_skill};

pub fn run(ctx: &Ctx, path: Option<PathBuf>, max_chars: usize) -> Result<()> {
    let target = path.unwrap_or_else(|| PathBuf::from("."));
    let config = LintConfig {
        soft_char_limit: max_chars,
    };

    let outcome = validate_skill(&target);

    if !outcome.is_ok() {
        if ctx.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&JsonOutput {
                    ok: false,
                    path: target.display().to_string(),
                    validation_errors: &outcome.errors,
                    warnings: &[],
                })?
            );
        } else {
            for err in &outcome.errors {
                ctx.warn(err.to_string());
            }
        }
        bail!(
            "`{}` cannot be linted ({} validation error{}); run `agentstack skill validate {}` for details",
            target.display(),
            outcome.errors.len(),
            if outcome.errors.len() == 1 { "" } else { "s" },
            target.display()
        );
    }

    let parsed = outcome
        .parsed
        .as_ref()
        .ok_or_else(|| anyhow!("skill validated but no parsed SKILL.md was available"))?;
    let content = outcome.content.as_deref().unwrap_or_default();
    let warnings = lint_skill(&target, parsed, content, &config);

    if ctx.json {
        let ok = warnings.is_empty();
        println!(
            "{}",
            serde_json::to_string_pretty(&JsonOutput {
                ok,
                path: target.display().to_string(),
                validation_errors: &[],
                warnings: &warnings,
            })?
        );
        if !ok {
            bail!(
                "`{}` has {} lint warning{}",
                target.display(),
                warnings.len(),
                if warnings.len() == 1 { "" } else { "s" }
            );
        }
        return Ok(());
    }

    for w in &warnings {
        ctx.say(format!("warning[{}]: {}", w.code, w.message));
    }
    if warnings.is_empty() {
        ctx.say(format!("ok ({}) — 0 warnings", target.display()));
        return Ok(());
    }
    bail!(
        "`{}` has {} lint warning{}",
        target.display(),
        warnings.len(),
        if warnings.len() == 1 { "" } else { "s" }
    );
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    ok: bool,
    path: String,
    validation_errors: &'a [ValidationError],
    warnings: &'a [LintWarning],
}
