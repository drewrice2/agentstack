//! `agentstack skill adopt` — bulk-import existing local skills into the
//! registry as candidate versions.
//!
//! Discovery reuses the `skill scan` machinery ([`discover_skills`]) and each
//! upload reuses the `skill push` pipeline (validate -> pack -> push). The
//! handler is split into two layers so the workflow can be unit-tested
//! against a [`MockRegistryClient`] without going through the binary:
//!
//! - [`run`] resolves the active registry + token and constructs a
//!   real HTTP registry client for live uploads.
//! - [`run_with_client`] takes any [`RegistryClient`] and runs the full
//!   scan -> validate -> push/dry-run batch.
//!
//! Per-skill push failures do not abort the batch; the command exits nonzero
//! only when every attempted push failed or a systemic error (auth, registry
//! unreachable) occurred.
//!
//! [`MockRegistryClient`]: crate::registry::MockRegistryClient

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use super::client::configured_client;
use super::push::{self, PushOptions, PushOutcome};
use super::refs;
use crate::output::Ctx;
use crate::registry::{RegistryClient, Visibility};
use crate::skill::{ValidationOutcome, discover_skills};

pub struct Args {
    pub path: Option<PathBuf>,
    pub org: Option<String>,
    pub visibility: String,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub dry_run: bool,
    pub yes: bool,
}

/// Resolved CLI args — kept distinct from [`Args`] so unit tests can build
/// the typed shape directly without going through clap.
pub struct AdoptOptions<'a> {
    pub root: &'a Path,
    pub org: &'a str,
    pub visibility: Visibility,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct AdoptOutcome {
    pub org: String,
    pub path: PathBuf,
    pub dry_run: bool,
    pub adopted: Vec<AdoptedSkill>,
    pub skipped: Vec<AdoptSkipped>,
    pub failed: Vec<AdoptFailed>,
}

#[derive(Debug, Clone)]
pub struct AdoptedSkill {
    pub name: String,
    pub skill_ref: String,
    pub version: String,
    pub audit_event_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AdoptSkipped {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct AdoptFailed {
    pub name: String,
    pub reason: String,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let root = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let visibility: Visibility = args.visibility.parse()?;
    let org = resolve_org(ctx, args.org.as_deref())?;
    crate::skill::check_slug(&org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
    push::validate_team_arg(visibility, args.team.as_deref())?;

    let plan = build_adopt_plan(&root)?;
    if !ctx.json {
        render_plan(ctx, &plan, &org);
    }

    let opts = AdoptOptions {
        root: &plan.root,
        org: &org,
        visibility,
        team: args.team,
        platforms: args.platforms,
        dry_run: args.dry_run,
    };

    if plan.queued.is_empty() {
        let outcome = execute_with_client(None, None, &plan, &opts, None);
        render_outcome(ctx, &outcome)?;
        return Ok(());
    }

    if !args.dry_run && !args.yes {
        let confirmed = ctx.prompt_confirm(
            format!(
                "Push {} skill(s) to {org} as candidate versions?",
                plan.queued.len()
            ),
            "skill adopt cannot prompt in this context; rerun with `--yes` or run interactively",
        )?;
        if !confirmed {
            ctx.say("no changes made");
            return Ok(());
        }
    }

    let outcome = if args.dry_run {
        execute_with_client(None, None, &plan, &opts, Some(ctx))
    } else {
        let configured = configured_client()?;
        ctx.verbose(format!("registry: {}", configured.url));
        execute_with_client(
            Some(&configured.client),
            Some(&configured.url),
            &plan,
            &opts,
            Some(ctx),
        )
    };

    render_outcome(ctx, &outcome)?;
    if all_attempted_pushes_failed(&outcome) {
        bail!(
            "skill adopt failed for all {} attempted skill{}",
            outcome.failed.len(),
            if outcome.failed.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Batch workflow used by integration tests. It performs discovery,
/// validation, and sequential push attempts, but leaves CLI prompting and
/// rendering to [`run`].
pub fn run_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    opts: AdoptOptions<'_>,
) -> Result<AdoptOutcome> {
    crate::skill::check_slug(opts.org)
        .map_err(|reason| anyhow!("invalid --org `{}`: {reason}", opts.org))?;
    push::validate_team_arg(opts.visibility, opts.team.as_deref())?;
    let plan = build_adopt_plan(opts.root)?;
    Ok(execute_with_client(
        client,
        registry_url,
        &plan,
        &opts,
        None,
    ))
}

fn resolve_org(ctx: &Ctx, explicit_org: Option<&str>) -> Result<String> {
    if let Some(org) = explicit_org {
        return Ok(org.to_string());
    }
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    refs::resolve_token_org(ctx, &configured.client, "skill adopt")
}

#[derive(Debug)]
struct AdoptPlan {
    root: PathBuf,
    queued: Vec<QueuedSkill>,
    skipped: Vec<AdoptSkipped>,
}

#[derive(Debug)]
struct QueuedSkill {
    name: String,
    path: PathBuf,
}

fn build_adopt_plan(root: &Path) -> Result<AdoptPlan> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to read `{}`", root.display()))?;
    let mut queued = Vec::new();
    let mut skipped = Vec::new();
    for skill in discover_skills(&root)? {
        // Discovery follows symlinks; refuse to upload anything that resolves
        // outside the adopt root so the plan never understates what gets
        // pushed.
        match skill.path.canonicalize() {
            Ok(resolved) if resolved.starts_with(&root) => {}
            Ok(resolved) => {
                skipped.push(AdoptSkipped {
                    path: skill.path,
                    reason: format!("resolves outside the adopt root (`{}`)", resolved.display()),
                });
                continue;
            }
            Err(err) => {
                skipped.push(AdoptSkipped {
                    path: skill.path,
                    reason: format!("failed to resolve path: {err}"),
                });
                continue;
            }
        }
        if skill.validation.is_ok() {
            queued.push(QueuedSkill {
                name: skill.name,
                path: skill.path,
            });
        } else {
            skipped.push(AdoptSkipped {
                path: skill.path,
                reason: validation_reasons(&skill.validation),
            });
        }
    }
    Ok(AdoptPlan {
        root,
        queued,
        skipped,
    })
}

fn validation_reasons(validation: &ValidationOutcome) -> String {
    let reasons: Vec<String> = validation.errors.iter().map(ToString::to_string).collect();
    if reasons.is_empty() {
        "validation_error".to_string()
    } else {
        reasons.join("; ")
    }
}

fn execute_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    plan: &AdoptPlan,
    opts: &AdoptOptions<'_>,
    ctx: Option<&Ctx>,
) -> AdoptOutcome {
    let mut outcome = AdoptOutcome {
        org: opts.org.to_string(),
        path: plan.root.clone(),
        dry_run: opts.dry_run,
        adopted: Vec::new(),
        skipped: plan.skipped.clone(),
        failed: Vec::new(),
    };
    let total = plan.queued.len();

    for (index, skill) in plan.queued.iter().enumerate() {
        let result = push::run_with_client(
            client,
            registry_url,
            PushOptions {
                source: &skill.path,
                org: opts.org,
                visibility: opts.visibility,
                team: opts.team.as_deref(),
                platforms: opts.platforms.clone(),
                dry_run: opts.dry_run,
            },
        );
        match result {
            Ok(pushed) => {
                render_progress(
                    ctx,
                    index + 1,
                    total,
                    &skill.name,
                    Ok(&pushed),
                    opts.dry_run,
                );
                outcome.adopted.push(AdoptedSkill {
                    name: skill.name.clone(),
                    skill_ref: pushed.skill_ref,
                    version: pushed.version,
                    audit_event_id: pushed.audit_event_id,
                });
            }
            Err(err) => {
                let reason = format!("{err:#}");
                render_progress(
                    ctx,
                    index + 1,
                    total,
                    &skill.name,
                    Err(reason.as_str()),
                    opts.dry_run,
                );
                outcome.failed.push(AdoptFailed {
                    name: skill.name.clone(),
                    reason,
                });
            }
        }
    }

    outcome
}

/// True when at least one push was attempted and none succeeded. Skipped
/// (invalid) skills alone never fail the batch.
fn all_attempted_pushes_failed(outcome: &AdoptOutcome) -> bool {
    !outcome.dry_run && outcome.adopted.is_empty() && !outcome.failed.is_empty()
}

fn render_plan(ctx: &Ctx, plan: &AdoptPlan, org: &str) {
    if plan.queued.is_empty() && plan.skipped.is_empty() {
        ctx.say(format!("no skills found under `{}`", plan.root.display()));
        return;
    }
    ctx.say(format!(
        "adopt plan for `{}` -> org {org}",
        plan.root.display()
    ));
    for skill in &plan.queued {
        ctx.say(format!("  {}  push as candidate", skill.name));
    }
    for skipped in &plan.skipped {
        ctx.say(format!(
            "  {}  skipped: {}",
            skipped.path.display(),
            skipped.reason
        ));
    }
    ctx.say(format!(
        "plan: {} to push, {} skipped",
        plan.queued.len(),
        plan.skipped.len()
    ));
}

fn render_progress(
    ctx: Option<&Ctx>,
    index: usize,
    total: usize,
    name: &str,
    result: Result<&PushOutcome, &str>,
    dry_run: bool,
) {
    let Some(ctx) = ctx else {
        return;
    };
    if ctx.json {
        return;
    }
    match result {
        Ok(outcome) if dry_run => ctx.say(format!(
            "[{index}/{total}] {name} ... would push {}",
            outcome.skill_ref
        )),
        Ok(outcome) => ctx.say(format!(
            "[{index}/{total}] {name} ... pushed {}",
            outcome.skill_ref
        )),
        Err(reason) => ctx.say(format!("[{index}/{total}] {name} ... failed: {reason}")),
    }
}

fn render_outcome(ctx: &Ctx, outcome: &AdoptOutcome) -> Result<()> {
    if ctx.json {
        ctx.say_always(render_json(outcome)?);
        return Ok(());
    }
    ctx.say(format!(
        "adopted {} · skipped {} · failed {}",
        outcome.adopted.len(),
        outcome.skipped.len(),
        outcome.failed.len()
    ));
    if outcome.dry_run {
        ctx.say("dry run; nothing uploaded");
    } else if !outcome.adopted.is_empty() {
        ctx.say("candidates: approve before readers install them.");
        ctx.say(format!(
            "next: agentstack skill version list {}/{}",
            outcome.org, outcome.adopted[0].name
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct AdoptJson<'a> {
    dry_run: bool,
    org: &'a str,
    path: String,
    adopted: Vec<AdoptedJson<'a>>,
    skipped: Vec<SkippedJson<'a>>,
    failed: Vec<FailedJson<'a>>,
    summary: AdoptSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
}

#[derive(Serialize)]
struct AdoptedJson<'a> {
    name: &'a str,
    skill_ref: &'a str,
    version: &'a str,
    audit_event_id: Option<&'a str>,
}

#[derive(Serialize)]
struct SkippedJson<'a> {
    path: String,
    reason: &'a str,
}

#[derive(Serialize)]
struct FailedJson<'a> {
    name: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct AdoptSummary {
    adopted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_adopt: Option<usize>,
    skipped: usize,
    failed: usize,
}

fn render_json(outcome: &AdoptOutcome) -> Result<String> {
    let empty =
        outcome.adopted.is_empty() && outcome.skipped.is_empty() && outcome.failed.is_empty();
    let out = AdoptJson {
        dry_run: outcome.dry_run,
        org: &outcome.org,
        path: outcome.path.display().to_string(),
        adopted: outcome
            .adopted
            .iter()
            .map(|row| AdoptedJson {
                name: &row.name,
                skill_ref: &row.skill_ref,
                version: &row.version,
                audit_event_id: row.audit_event_id.as_deref(),
            })
            .collect(),
        skipped: outcome
            .skipped
            .iter()
            .map(|row| SkippedJson {
                path: row.path.display().to_string(),
                reason: &row.reason,
            })
            .collect(),
        failed: outcome
            .failed
            .iter()
            .map(|row| FailedJson {
                name: &row.name,
                reason: &row.reason,
            })
            .collect(),
        summary: AdoptSummary {
            adopted: outcome.adopted.len(),
            would_adopt: outcome.dry_run.then_some(outcome.adopted.len()),
            skipped: outcome.skipped.len(),
            failed: outcome.failed.len(),
        },
        empty_message: empty.then(|| format!("no skills found under `{}`", outcome.path.display())),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(dry_run: bool, adopted: usize, failed: usize) -> AdoptOutcome {
        AdoptOutcome {
            org: "acme".to_string(),
            path: PathBuf::from("."),
            dry_run,
            adopted: (0..adopted)
                .map(|i| AdoptedSkill {
                    name: format!("skill-{i}"),
                    skill_ref: format!("acme/skill-{i}"),
                    version: "1".to_string(),
                    audit_event_id: None,
                })
                .collect(),
            skipped: Vec::new(),
            failed: (0..failed)
                .map(|i| AdoptFailed {
                    name: format!("skill-{i}"),
                    reason: "boom".to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn all_failed_exits_nonzero() {
        assert!(all_attempted_pushes_failed(&outcome(false, 0, 2)));
    }

    #[test]
    fn partial_failure_exits_zero() {
        assert!(!all_attempted_pushes_failed(&outcome(false, 1, 1)));
    }

    #[test]
    fn nothing_attempted_exits_zero() {
        assert!(!all_attempted_pushes_failed(&outcome(false, 0, 0)));
    }

    #[test]
    fn dry_run_never_fails_the_batch() {
        assert!(!all_attempted_pushes_failed(&outcome(true, 0, 2)));
    }
}
