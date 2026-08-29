//! `agentstack skill version approve <org>/<skill>@<version>` — promote an
//! uploaded candidate as the current approved version.

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::{
    client::{configured_client, registry_context},
    refs,
};
use crate::output::Ctx;
use crate::registry::{RegistryClient, SkillMetadata};
use crate::skill_ref::{SkillRef, check_version};

pub struct Args {
    pub skill_ref: String,
    pub team: Option<String>,
    pub version: Option<String>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let parsed = refs::validate_skill_ref_input_with_team(&args.skill_ref, args.team.as_deref())?;
    let version = match (parsed.version(), args.version.as_deref()) {
        (Some(inline), Some(flag)) if inline != flag => {
            bail!("skill ref pins version `{inline}`, but --version is `{flag}`")
        }
        (Some(inline), _) => inline.to_string(),
        (None, Some(flag)) => flag.to_string(),
        (None, None) => bail!("approve expects `skill@version` or `org/skill@version`"),
    };
    check_version(&version)?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let resolved = refs::resolve_skill_ref_with_team(
        ctx,
        &configured.client,
        &args.skill_ref,
        args.team.as_deref(),
    )?;
    let skill_ref = SkillRef::new(resolved.org, resolved.name)?;
    run_with_client(
        &configured.client,
        Some(&configured.url),
        &skill_ref,
        &version,
        ctx.json,
        ctx.quiet,
    )
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    skill_ref: &SkillRef,
    version: &str,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let metadata = client
        .approve(skill_ref, version)
        .with_context(|| registry_context(registry_url, "approve request to", "approve request"))?;
    if metadata.current != Some(true) {
        bail!(
            "approve response for `{}` did not mark the version as current=true",
            metadata.skill_ref()
        );
    }

    if json {
        println!("{}", render_json(&metadata)?);
    } else if !quiet {
        render_human(&metadata, registry_url);
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    skill_ref: String,
    metadata: &'a SkillMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<&'a str>,
    next_commands: Vec<String>,
    next_command_templates: Vec<String>,
}

fn render_json(metadata: &SkillMetadata) -> Result<String> {
    let out = JsonOutput {
        skill_ref: metadata.skill_ref(),
        metadata,
        audit_event_id: audit_event_id(metadata),
        next_commands: approve_next_commands(metadata),
        next_command_templates: approve_next_command_templates(metadata),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn approve_next_commands(metadata: &SkillMetadata) -> Vec<String> {
    let unversioned_ref = format!("{}/{}", metadata.org, metadata.name);
    let mut commands = vec![format!("agentstack skill status {unversioned_ref}")];
    if let Some(audit_event_id) = audit_event_id(metadata) {
        commands.push(format!(
            "agentstack audit show {audit_event_id} --org {}",
            metadata.org
        ));
    }
    commands
}

fn approve_next_command_templates(metadata: &SkillMetadata) -> Vec<String> {
    vec![format!(
        "agentstack skill install {} --target <target>",
        metadata.skill_ref()
    )]
}

fn audit_event_id(metadata: &SkillMetadata) -> Option<&str> {
    metadata.audit_event_id.as_deref()
}

fn render_human(metadata: &SkillMetadata, registry_url: Option<&str>) {
    let unversioned_ref = format!("{}/{}", metadata.org, metadata.name);
    println!("approved {} as current", metadata.skill_ref());
    println!(
        "  status:     {}",
        metadata
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "approved".to_string())
    );
    println!(
        "  current:    {}",
        if metadata.current == Some(true) {
            "yes"
        } else {
            "no"
        }
    );
    println!("  hash:       {}", metadata.hash.hex);
    if let Some(audit_event_id) = metadata.audit_event_id.as_deref() {
        println!("  audit_event_id: {audit_event_id}");
    }
    if let Some(url) = registry_url {
        println!("  registry:   {url}");
    }
    println!();
    println!("next:");
    println!("  agentstack skill status {unversioned_ref}");
    println!(
        "  agentstack skill install {} --target <target>",
        metadata.skill_ref()
    );
    if let Some(audit_event_id) = metadata.audit_event_id.as_deref() {
        println!(
            "  agentstack audit show {audit_event_id} --org {}",
            metadata.org
        );
    }
}
