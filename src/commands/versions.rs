//! `agentstack skill version list <org>/<skill>` — list every uploaded version.

use anyhow::{Context, Result};
use serde::Serialize;

use super::{
    client::{configured_client, registry_context},
    refs,
};
use crate::output::Ctx;
use crate::registry::{RegistryClient, VersionInfo};
use crate::skill_ref::SkillRef;

pub struct Args {
    pub skill_ref: String,
    pub team: Option<String>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    refs::validate_skill_ref_input_with_team(&args.skill_ref, args.team.as_deref())?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let mut skill_ref = refs::resolve_skill_ref_with_team(
        ctx,
        &configured.client,
        &args.skill_ref,
        args.team.as_deref(),
    )?;
    // Drop any inline @version — `versions` always lists the full set.
    skill_ref.version = None;
    run_with_client(
        &configured.client,
        Some(&configured.url),
        &skill_ref,
        ctx.json,
        ctx.quiet,
    )
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    skill_ref: &SkillRef,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let versions = client.list_versions(skill_ref).with_context(|| {
        registry_context(registry_url, "versions request to", "versions request")
    })?;

    if json {
        println!("{}", render_json(skill_ref, &versions)?);
        return Ok(());
    }

    println!("{}", skill_ref.unversioned());
    if versions.is_empty() {
        println!("{}", empty_message(skill_ref));
        if !quiet {
            println!("next: {}", empty_next_command(skill_ref));
        }
        return Ok(());
    }

    let current = versions
        .iter()
        .find(|version| version.current.unwrap_or(false));
    match current {
        Some(version) => println!("current: {}", version_summary(version)),
        None => {
            println!("current: none");
            if !quiet && let Some(latest) = versions.first() {
                println!(
                    "no approved current version yet; ask an admin to run: agentstack skill version approve {}@{}",
                    skill_ref.unversioned(),
                    latest.version
                );
            }
        }
    }
    if let Some(latest) = versions.first() {
        println!("latest:  {}", version_summary(latest));
    }
    println!();

    let version_width = versions
        .iter()
        .map(|v| format!("v{}", v.version).len())
        .max()
        .unwrap_or(0)
        .max("VERSION".len());
    let status_width = versions
        .iter()
        .map(|v| lifecycle(v).len())
        .max()
        .unwrap_or(0)
        .max("STATUS".len());
    println!(
        "{:<vw$}  {:<sw$}  CURRENT  HASH      CREATED",
        "VERSION",
        "STATUS",
        vw = version_width,
        sw = status_width
    );
    for v in &versions {
        let created = v.created_at.as_deref().unwrap_or("-");
        let lifecycle = lifecycle(v);
        let current = if v.current.unwrap_or(false) {
            "yes"
        } else {
            "-"
        };
        println!(
            "{ver:<vw$}  {lifecycle:<sw$}  {current:<7}  {short}  {created}",
            ver = format!("v{}", v.version),
            lifecycle = lifecycle,
            current = current,
            short = v.hash.short(),
            vw = version_width,
            sw = status_width,
        );
        if let Some(reason) = v.yank_reason.as_deref() {
            println!("  yanked: {reason}");
        }
        if let Some(reason) = v.deprecation_reason.as_deref() {
            println!("  deprecated: {reason}");
        }
    }
    if !quiet
        && current
            .map(|version| version.yanked_at.is_none())
            .unwrap_or(false)
    {
        println!();
        println!(
            "install current: agentstack skill install {} --target <target>",
            skill_ref.unversioned()
        );
    }
    Ok(())
}

fn version_summary(version: &VersionInfo) -> String {
    format!("v{} {}", version.version, lifecycle(version))
}

fn lifecycle(version: &VersionInfo) -> String {
    if version.yanked_at.is_some() {
        "yanked".to_string()
    } else if version.deprecated_at.is_some() {
        "deprecated".to_string()
    } else {
        version
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "-".to_string())
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    skill_ref: String,
    versions: &'a [VersionInfo],
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
}

fn render_json(skill_ref: &SkillRef, versions: &[VersionInfo]) -> Result<String> {
    let out = JsonOutput {
        skill_ref: skill_ref.unversioned(),
        versions,
        empty_message: versions.is_empty().then(|| empty_message(skill_ref)),
        next_command: versions.is_empty().then(|| empty_next_command(skill_ref)),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn empty_message(skill_ref: &SkillRef) -> String {
    format!(
        "no uploaded versions found for `{}`.",
        skill_ref.unversioned()
    )
}

fn empty_next_command(skill_ref: &SkillRef) -> String {
    format!("agentstack skill list --org {}", skill_ref.org)
}
