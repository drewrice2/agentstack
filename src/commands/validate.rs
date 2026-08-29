use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::error::CliError;
use crate::output::Ctx;
use crate::skill::{ValidationError, ValidationOutcome, validate_skill};

pub fn run(ctx: &Ctx, path: Option<PathBuf>) -> Result<()> {
    let target = path.unwrap_or_else(|| PathBuf::from("."));
    let outcome = validate_skill(&target);

    if outcome.is_ok() {
        if ctx.json {
            let payload = ValidateJson::from_outcome(&target, &outcome);
            println!("{}", serde_json::to_string_pretty(&payload)?);
            return Ok(());
        }
        if let Some(parsed) = &outcome.parsed
            && let (Some(name), Some(description)) = (
                parsed.raw_manifest.name.as_deref(),
                parsed.raw_manifest.description.as_deref(),
            )
        {
            ctx.say(format!("skill: {name} — {description}"));
        }
        ctx.say(format!("ok ({})", target.display()));
        return Ok(());
    }

    let message = format!(
        "`{}` is not a valid skill ({} error{})",
        target.display(),
        outcome.errors.len(),
        if outcome.errors.len() == 1 { "" } else { "s" }
    );

    if ctx.json {
        let code = outcome
            .errors
            .first()
            .map(|error| error.code.as_str())
            .unwrap_or("validation_failed");
        return Err(CliError::new(code, message)
            .resource(target.display().to_string())
            .action("validate_skill")
            .into());
    }

    for err in &outcome.errors {
        ctx.warn(err.to_string());
    }
    bail!(message);
}

#[derive(Serialize)]
struct ValidateJson<'a> {
    ok: bool,
    path: String,
    name: Option<&'a str>,
    description: Option<&'a str>,
    errors: &'a [ValidationError],
}

impl<'a> ValidateJson<'a> {
    fn from_outcome(path: &Path, outcome: &'a ValidationOutcome) -> Self {
        let parsed = outcome.parsed.as_ref();
        Self {
            ok: outcome.is_ok(),
            path: path.display().to_string(),
            name: parsed.and_then(|p| p.raw_manifest.name.as_deref()),
            description: parsed.and_then(|p| p.raw_manifest.description.as_deref()),
            errors: &outcome.errors,
        }
    }
}
