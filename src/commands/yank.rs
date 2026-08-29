//! Skill version yank/deprecate lifecycle handlers.

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{client::configured_client, refs};
use crate::output::Ctx;
use crate::registry::{RegistryClient, SkillMetadata};
use crate::skill_ref::{SkillRef, check_version};

pub struct Args {
    pub skill_ref: String,
    pub team: Option<String>,
    pub reason: String,
}

#[derive(Clone, Copy)]
pub enum Action {
    Yank,
    Deprecate,
}

impl Action {
    /// Past-tense state word for success output and the JSON `action` field.
    fn label(&self) -> &'static str {
        match self {
            Self::Yank => "yanked",
            Self::Deprecate => "deprecated",
        }
    }

    /// Imperative verb for error context (e.g. "yank request to <url> failed"),
    /// matching the phrasing used by `approve`.
    fn verb(&self) -> &'static str {
        match self {
            Self::Yank => "yank",
            Self::Deprecate => "deprecate",
        }
    }
}

pub struct YankOptions<'a> {
    pub registry_url: Option<&'a str>,
    pub skill_ref: &'a SkillRef,
    pub version: &'a str,
    pub reason: &'a str,
    pub action: Action,
    pub json: bool,
    pub quiet: bool,
}

pub fn run(ctx: &Ctx, args: Args, action: Action) -> Result<()> {
    if args.reason.trim().is_empty() {
        bail!("--reason must not be empty");
    }
    let parsed = refs::validate_skill_ref_input_with_team(&args.skill_ref, args.team.as_deref())?;
    if match &parsed {
        refs::SkillRefInput::Qualified(skill_ref) => skill_ref.version.is_none(),
        refs::SkillRefInput::Relative { version, .. } => version.is_none(),
    } {
        bail!("skill ref must include `@<version>`");
    }
    if let Some(version) = parsed.version() {
        check_version(version)?;
    }
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let skill_ref = refs::resolve_skill_ref_with_team(
        ctx,
        &configured.client,
        &args.skill_ref,
        args.team.as_deref(),
    )?;
    let version = skill_ref
        .version
        .clone()
        .ok_or_else(|| anyhow::anyhow!("skill ref must include `@<version>`"))?;
    check_version(&version)?;
    let unversioned = skill_ref.clone();
    let unversioned = SkillRef::new(unversioned.org, unversioned.name)?;

    run_with_client(
        &configured.client,
        YankOptions {
            registry_url: Some(&configured.url),
            skill_ref: &unversioned,
            version: &version,
            reason: &args.reason,
            action,
            json: ctx.json,
            quiet: ctx.quiet,
        },
    )
}

pub fn run_with_client(client: &dyn RegistryClient, opts: YankOptions<'_>) -> Result<()> {
    let metadata = match opts.action {
        Action::Yank => client.yank(opts.skill_ref, opts.version, opts.reason),
        Action::Deprecate => client.deprecate(opts.skill_ref, opts.version, opts.reason),
    }
    .with_context(|| match opts.registry_url {
        Some(url) => format!("{} request to {url} failed", opts.action.verb()),
        None => format!("{} request failed", opts.action.verb()),
    })?;

    if opts.json {
        println!("{}", render_json(&metadata, &opts.action)?);
    } else if !opts.quiet {
        render_human(&metadata, &opts.action, opts.registry_url);
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    skill_ref: String,
    action: &'a str,
    metadata: &'a SkillMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<&'a str>,
    next_commands: Vec<String>,
}

fn render_json(metadata: &SkillMetadata, action: &Action) -> Result<String> {
    let out = JsonOutput {
        skill_ref: metadata.skill_ref(),
        action: action.label(),
        metadata,
        audit_event_id: metadata.audit_event_id.as_deref(),
        next_commands: lifecycle_next_commands(metadata),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn lifecycle_next_commands(metadata: &SkillMetadata) -> Vec<String> {
    let unversioned_ref = format!("{}/{}", metadata.org, metadata.name);
    let mut commands = vec![format!("agentstack skill status {unversioned_ref}")];
    if let Some(audit_event_id) = metadata.audit_event_id.as_deref() {
        commands.push(format!(
            "agentstack audit show {audit_event_id} --org {}",
            metadata.org
        ));
    }
    commands
}

fn render_human(metadata: &SkillMetadata, action: &Action, registry_url: Option<&str>) {
    println!("{} {}", action.label(), metadata.skill_ref());
    let reason = match action {
        Action::Yank => metadata.yank_reason.as_deref(),
        Action::Deprecate => metadata.deprecation_reason.as_deref(),
    };
    if let Some(reason) = reason {
        println!("  reason:     {reason}");
    }
    let timestamp = match action {
        Action::Yank => metadata.yanked_at.as_deref(),
        Action::Deprecate => metadata.deprecated_at.as_deref(),
    };
    if let Some(ts) = timestamp {
        println!("  at:         {ts}");
    }
    if let Some(audit_event_id) = metadata.audit_event_id.as_deref() {
        println!("  audit_event_id: {audit_event_id}");
    }
    if let Some(url) = registry_url {
        println!("  registry:   {url}");
    }
}
