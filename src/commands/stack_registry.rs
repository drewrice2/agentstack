//! Registry stack management handlers.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use super::client::configured_client;
use crate::output::Ctx;
use crate::registry::{
    RegistryClient, StackDetail, StackListFilters, StackResolve, StackSummary, VersionPolicy,
};
use crate::skill::check_slug;
use crate::skill_ref::check_version;

/// Registry stack operation, fully resolved to an org-qualified form by the
/// dispatch layer in [`super`]. This is not parsed by clap directly.
#[derive(Debug)]
pub enum StackRegistryCommand {
    /// Create a new stack in an organization.
    Create {
        stack: String,
        org: String,
        visibility: String,
        team: Option<String>,
        name: Option<String>,
        description: String,
    },
    /// Add or update a skill in a stack.
    Add {
        stack: String,
        skill: String,
        org: String,
        version_policy: Option<String>,
        pin_version: Option<String>,
    },
    /// Remove a skill from a stack.
    Remove {
        stack: String,
        skill: String,
        org: String,
        yes: bool,
        dry_run: bool,
    },
    /// Inspect one stack and its items.
    Inspect { stack: String, org: String },
    /// List visible stacks in one org.
    List {
        org: String,
        team: Option<String>,
        owner: Option<String>,
        limit: Option<usize>,
    },
    /// Resolve one stack to concrete skill versions.
    Resolve { stack: String, org: String },
}

pub fn run(ctx: &Ctx, action: StackRegistryCommand) -> Result<()> {
    validate_action(&action)?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    if let StackRegistryCommand::Remove {
        stack,
        skill,
        org,
        yes,
        dry_run,
    } = &action
        && !dry_run
        && !yes
    {
        let confirmed = ctx.prompt_confirm(
            format!("Remove skill `{skill}` from stack `{org}/{stack}`?"),
            "stack remove cannot prompt in this context; rerun with `--yes` or run interactively",
        )?;
        if !confirmed {
            ctx.say("no changes made");
            return Ok(());
        }
    }
    run_with_client(&configured.client, action, ctx.json, ctx.quiet)
}

fn validate_action(action: &StackRegistryCommand) -> Result<()> {
    match action {
        StackRegistryCommand::Create {
            stack,
            org,
            visibility,
            team,
            ..
        } => {
            validate_org_stack(org, stack)?;
            let visibility = visibility.parse::<crate::registry::Visibility>()?;
            if visibility == crate::registry::Visibility::Team && team.is_none() {
                bail!("--team is required when --scope team is used");
            }
            if visibility != crate::registry::Visibility::Team && team.is_some() {
                bail!("--team can only be used with --scope team");
            }
            if let Some(team) = team {
                check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
            }
        }
        StackRegistryCommand::Add {
            stack,
            skill,
            org,
            version_policy,
            pin_version,
        } => {
            validate_org_stack(org, stack)?;
            check_slug(skill).map_err(|reason| anyhow!("invalid skill `{skill}`: {reason}"))?;
            if let Some(policy) = version_policy {
                policy.parse::<VersionPolicy>()?;
            }
            if let Some(version) = pin_version {
                check_version(version)?;
            }
            stack_item_policy(version_policy.as_deref(), pin_version.as_deref())?;
        }
        StackRegistryCommand::Remove {
            stack, skill, org, ..
        } => {
            validate_org_stack(org, stack)?;
            check_slug(skill).map_err(|reason| anyhow!("invalid skill `{skill}`: {reason}"))?;
        }
        StackRegistryCommand::Inspect { stack, org }
        | StackRegistryCommand::Resolve { stack, org } => {
            validate_org_stack(org, stack)?;
        }
        StackRegistryCommand::List { org, team, .. } => {
            check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
            if let Some(team) = team {
                check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
            }
        }
    }
    Ok(())
}

fn validate_org_stack(org: &str, stack: &str) -> Result<()> {
    check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
    check_slug(stack).map_err(|reason| anyhow!("invalid stack `{stack}`: {reason}"))?;
    Ok(())
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    action: StackRegistryCommand,
    json: bool,
    quiet: bool,
) -> Result<()> {
    match action {
        StackRegistryCommand::Create {
            stack,
            org,
            visibility,
            team,
            name,
            description,
        } => {
            let visibility = visibility.parse()?;
            let detail = client
                .create_stack(
                    &org,
                    &stack,
                    name.as_deref().unwrap_or(&stack),
                    &description,
                    visibility,
                    team.as_deref(),
                )
                .with_context(|| format!("create stack {org}/{stack} failed"))?;
            print_detail(&detail, json, quiet, "created")?;
        }
        StackRegistryCommand::Add {
            stack,
            skill,
            org,
            version_policy,
            pin_version,
        } => {
            let policy = stack_item_policy(version_policy.as_deref(), pin_version.as_deref())?;
            let detail = client
                .upsert_stack_item(&org, &stack, &skill, policy, pin_version.as_deref())
                .with_context(|| format!("add {org}/{skill} to stack {org}/{stack} failed"))?;
            print_detail(&detail, json, quiet, "stack")?;
        }
        StackRegistryCommand::Remove {
            stack,
            skill,
            org,
            yes: _,
            dry_run,
        } => {
            if dry_run {
                let detail = client
                    .inspect_stack(&org, &stack)
                    .with_context(|| format!("inspect stack {org}/{stack} failed"))?;
                if !detail.items.iter().any(|item| item.skill == skill) {
                    bail!("skill `{skill}` is not in stack `{org}/{stack}`; nothing to remove");
                }
                print_remove_dry_run(&detail, &skill, json, quiet)?;
            } else {
                let detail = client
                    .remove_stack_item(&org, &stack, &skill)
                    .with_context(|| {
                        format!("remove {org}/{skill} from stack {org}/{stack} failed")
                    })?;
                print_detail(&detail, json, quiet, "stack")?;
            }
        }
        StackRegistryCommand::Inspect { stack, org } => {
            let detail = client
                .inspect_stack(&org, &stack)
                .with_context(|| format!("inspect stack {org}/{stack} failed"))?;
            print_detail(&detail, json, quiet, "stack")?;
        }
        StackRegistryCommand::List {
            org,
            team,
            owner,
            limit,
        } => {
            let filters = StackListFilters { owner, team, limit };
            let stacks = client
                .list_stacks_with_filters(&org, &filters)
                .with_context(|| format!("list stacks in {org} failed"))?;
            print_list(&stacks, json, quiet, &filters, &org)?;
        }
        StackRegistryCommand::Resolve { stack, org } => {
            let resolved = client
                .resolve_stack(&org, &stack)
                .with_context(|| format!("resolve stack {org}/{stack} failed"))?;
            print_resolve(&resolved, json, quiet)?;
        }
    }
    Ok(())
}

fn stack_item_policy(
    version_policy: Option<&str>,
    pin_version: Option<&str>,
) -> Result<VersionPolicy> {
    if pin_version.is_some() {
        if let Some(policy) = version_policy
            && policy != "pinned"
        {
            bail!("--pin-version requires --version-policy pinned or no --version-policy");
        }
        return Ok(VersionPolicy::Pinned);
    }
    match version_policy {
        Some("pinned") => bail!("--version-policy pinned requires --pin-version <VERSION>"),
        Some(policy) => policy.parse(),
        None => Ok(VersionPolicy::Current),
    }
}

#[derive(Serialize)]
struct StackEnvelope<'a> {
    stack: &'a StackDetail,
}

#[derive(Serialize)]
struct StackRemoveDryRunJson<'a> {
    dry_run: bool,
    stack: &'a StackDetail,
    would_remove: &'a str,
    items_after: Vec<&'a str>,
}

#[derive(Serialize)]
struct StackListEnvelope<'a> {
    org: &'a str,
    filters: StackListJsonFilters<'a>,
    stacks: &'a [StackSummary],
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
}

#[derive(Serialize)]
struct StackListJsonFilters<'a> {
    org: &'a str,
    team: Option<&'a str>,
    owner: Option<&'a str>,
    limit: Option<usize>,
}

fn print_detail(detail: &StackDetail, json: bool, quiet: bool, label: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StackEnvelope { stack: detail })?
        );
    } else if !quiet {
        println!("{label} {}/{}", detail.org, detail.slug);
        if let Some(owner_email) = &detail.owner_email {
            println!("  owner:      {owner_email}");
        }
        println!("  visibility: {}", detail.visibility);
        if let Some(team) = &detail.team {
            println!("  team:       {team}");
        }
        println!("  items:      {}", detail.items.len());
        if let Some(audit_event_id) = &detail.audit_event_id {
            println!("  audit_event_id: {audit_event_id}");
        }
        for item in &detail.items {
            let policy = item
                .pinned_version
                .as_ref()
                .map(|version| format!("{} @{version}", item.version_policy))
                .unwrap_or_else(|| item.version_policy.to_string());
            println!("  - {} ({policy})", item.skill);
        }
    }
    Ok(())
}

fn print_remove_dry_run(detail: &StackDetail, skill: &str, json: bool, quiet: bool) -> Result<()> {
    let items_after: Vec<&str> = detail
        .items
        .iter()
        .map(|item| item.skill.as_str())
        .filter(|item| *item != skill)
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StackRemoveDryRunJson {
                dry_run: true,
                stack: detail,
                would_remove: skill,
                items_after,
            })?
        );
    } else if !quiet {
        println!(
            "would remove skill `{skill}` from stack `{}/{}`",
            detail.org, detail.slug
        );
        println!("  items after: {}", items_after.len());
        for item in &detail.items {
            if item.skill == skill {
                continue;
            }
            let policy = item
                .pinned_version
                .as_ref()
                .map(|version| format!("{} @{version}", item.version_policy))
                .unwrap_or_else(|| item.version_policy.to_string());
            println!("  - {} ({policy})", item.skill);
        }
        println!("dry run; nothing removed.");
    }
    Ok(())
}

fn print_list(
    stacks: &[StackSummary],
    json: bool,
    quiet: bool,
    filters: &StackListFilters,
    org: &str,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&StackListEnvelope {
                org,
                filters: StackListJsonFilters {
                    org,
                    team: filters.team.as_deref(),
                    owner: filters.owner.as_deref(),
                    limit: filters.limit,
                },
                stacks,
                empty_message: stacks.is_empty().then(|| empty_stack_message(org, filters)),
                next_command: stacks
                    .is_empty()
                    .then(|| stack_discovery_suggestion(org, filters))
                    .filter(|command| crate::output::is_concrete_next_command(command)),
            })?
        );
    } else {
        if stacks.is_empty() {
            println!("{}", empty_stack_message(org, filters));
            if !quiet {
                println!("next: {}", stack_discovery_suggestion(org, filters));
            }
            return Ok(());
        }
        if !quiet {
            println!("{}", stack_summary(stacks.len(), org, filters));
        }
        for stack in stacks {
            println!(
                "{}/{}  {}  {} item(s)",
                stack.org, stack.slug, stack.visibility, stack.item_count
            );
            if let Some(owner_email) = &stack.owner_email {
                println!("  owner: {owner_email}");
            }
            if let Some(team) = &stack.team {
                println!("  team:  {team}");
            }
        }
    }
    Ok(())
}

fn stack_summary(count: usize, org: &str, filters: &StackListFilters) -> String {
    let mut parts = vec![format!("org={org}")];
    if let Some(team) = filters.team.as_deref() {
        parts.push(format!("team={team}"));
    }
    if let Some(owner) = filters.owner.as_deref() {
        parts.push(format!("owner={owner}"));
    }
    if let Some(limit) = filters.limit {
        parts.push(format!("limit={limit}"));
    }
    format!("showing {count} stack(s) ({})", parts.join(", "))
}

fn empty_stack_message(org: &str, filters: &StackListFilters) -> String {
    let mut message = format!("no stacks found in `{org}`");
    let mut qualifiers = Vec::new();
    if let Some(team) = filters.team.as_deref() {
        qualifiers.push(format!("team `{team}`"));
    }
    if let Some(owner) = filters.owner.as_deref() {
        qualifiers.push(format!("owner `{owner}`"));
    }
    if !qualifiers.is_empty() {
        message.push_str(" matching ");
        message.push_str(&qualifiers.join(", "));
    }
    message.push('.');
    message
}

fn stack_discovery_suggestion(org: &str, filters: &StackListFilters) -> String {
    if filters.team.is_some() || filters.owner.is_some() {
        return format!("agentstack stack list --org {org}");
    }
    format!("agentstack team list --org {org}")
}

fn print_resolve(resolved: &StackResolve, json: bool, quiet: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(resolved)?);
    } else if !quiet {
        println!(
            "resolved {}/{} ({} item(s))",
            resolved.stack.org,
            resolved.stack.slug,
            resolved.items.len()
        );
        println!("  visibility: {}", resolved.stack.visibility);
        if let Some(team) = &resolved.stack.team {
            println!("  team:       {team}");
        }
        println!("  resolved:   {}", resolved.resolved_at);
        println!(
            "  manifest:   {}",
            crate::receipt::format_hash(&resolved.manifest_hash)
        );
        for item in &resolved.items {
            println!(
                "  - {}@{} ({}, version_id: {}) {}",
                item.skill,
                item.version,
                item.version_policy,
                item.version_id,
                crate::receipt::format_hash(&item.archive_hash)
            );
        }
    }
    Ok(())
}
