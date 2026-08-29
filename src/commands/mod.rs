//! Top-level command dispatch. Each handler lives in its own submodule so
//! handler logic stays separate from CLI parsing.

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

use crate::cli::{
    AuditCommand, AuthCommand, Cli, Command, InstallCommand, SkillCommand, SkillVersionCommand,
    SkillVisibilityCommand, StackCommand, StackVisibilityCommand, TargetCommand,
};
use crate::output::Ctx;
use crate::registry::{AuditEvent, RegistryClient, SkillImpact, VersionInfo, Visibility};
use crate::skill_ref::SkillRef;

mod adopt;
mod approve;
mod cache;
mod candidates;
mod client;
mod completions;
mod config;
mod diff;
mod doctor;
mod init;
mod inspect;
mod install;
mod install_receipts;
mod lint;
mod list;
mod login;
mod logout;
mod pack;
mod push;
mod refs;
mod registry;
mod registry_export;
mod search;
mod security_scan;
mod setup;
mod stack_registry;
mod sync;
mod targets;
mod teams;
mod uninstall;
mod unpack;
mod update;
mod validate;
mod versions;
mod whoami;
mod yank;

pub fn dispatch(cli: Cli, ctx: &Ctx) -> Result<()> {
    match cli.command {
        Command::Skill { action } => dispatch_skill(ctx, action),
        Command::Stack { action } => dispatch_stack(ctx, action),
        Command::Install { action } => dispatch_install(ctx, action),
        Command::Auth { action } => dispatch_auth(ctx, action),
        Command::Target { action } => dispatch_target(ctx, action),
        Command::Audit { action } => dispatch_audit(ctx, action),
        Command::Team { action } => teams::run(ctx, action),
        Command::Config { action } => config::run(ctx, action),
        Command::Cache { action } => cache::run(ctx, action),
        Command::Registry { action } => registry::run(ctx, action),
        Command::Sync {
            manifest,
            check,
            prune,
            yes,
        } => sync::run(
            ctx,
            sync::Args {
                manifest,
                check,
                prune,
                yes,
            },
        ),
        Command::Doctor => doctor::run(ctx),
        Command::Completion { shell } => completions::run(shell),
    }
}

fn dispatch_skill(ctx: &Ctx, action: SkillCommand) -> Result<()> {
    match action {
        SkillCommand::Init {
            path,
            name,
            description,
        } => init::run(
            ctx,
            init::Args {
                path,
                name,
                description,
            },
        ),
        SkillCommand::Validate { path } => validate::run(ctx, path),
        SkillCommand::Lint { path, max_chars } => lint::run(ctx, path, max_chars),
        SkillCommand::Inspect { path, max_chars } => inspect::run(ctx, path, max_chars),
        SkillCommand::SecurityScan { path } => security_scan::run(ctx, path),
        SkillCommand::Scan { path } => list::run(
            ctx,
            list::Args {
                local: true,
                remote: false,
                org: None,
                team: None,
                platforms: Vec::new(),
                visibility: None,
                owner: None,
                sort: None,
                limit: None,
                path,
            },
        ),
        SkillCommand::Pack {
            path,
            out,
            force,
            no_cache,
        } => pack::run(
            ctx,
            pack::Args {
                path,
                out,
                force,
                no_cache,
            },
        ),
        SkillCommand::Unpack {
            archive,
            out,
            force,
        } => unpack::run(
            ctx,
            unpack::Args {
                archive,
                out,
                force,
            },
        ),
        SkillCommand::List {
            org,
            team,
            platforms,
            scope,
            owner,
            sort,
            limit,
        } => list::run(
            ctx,
            list::Args {
                local: false,
                remote: true,
                org,
                team,
                platforms,
                visibility: scope.map(|s| s.as_visibility_str().to_string()),
                owner,
                sort: sort.map(|s| s.as_sort_str().to_string()),
                limit,
                path: None,
            },
        ),
        SkillCommand::Search {
            query,
            org,
            team,
            platforms,
            scope,
            owner,
            sort,
            limit,
        } => search::run(
            ctx,
            search::Args {
                query,
                org,
                team,
                platforms,
                visibility: scope.map(|s| s.as_visibility_str().to_string()),
                owner,
                sort: sort.map(|s| s.as_sort_str().to_string()),
                limit,
            },
        ),
        SkillCommand::Candidates { org, limit } => {
            candidates::run(ctx, candidates::Args { org, limit })
        }
        SkillCommand::Show {
            skill_ref,
            team,
            target,
        } => {
            if let Some(target) = target {
                install_receipts::inspect(ctx, &skill_ref, None, &target)
            } else {
                skill_show(ctx, &skill_ref, team.as_deref())
            }
        }
        SkillCommand::Status { skill_ref, team } => skill_status(ctx, &skill_ref, team.as_deref()),
        SkillCommand::Impact { skill_ref, team } => skill_impact(ctx, &skill_ref, team.as_deref()),
        SkillCommand::Diff {
            left,
            right,
            target,
            allow_yanked,
        } => diff::run(
            ctx,
            diff::Args {
                left,
                right,
                target,
                allow_yanked,
            },
        ),
        SkillCommand::Push {
            path,
            all,
            org,
            scope,
            team,
            platforms,
            include,
            exclude,
            yes,
            dry_run,
        } => push::run(
            ctx,
            push::Args {
                path,
                org,
                visibility: scope.as_visibility_str().to_string(),
                team,
                platforms,
                dry_run,
                all,
                include,
                exclude,
                yes,
            },
        ),
        SkillCommand::Adopt {
            path,
            org,
            scope,
            team,
            platforms,
            dry_run,
            yes,
        } => adopt::run(
            ctx,
            adopt::Args {
                path,
                org,
                visibility: scope.as_visibility_str().to_string(),
                team,
                platforms,
                dry_run,
                yes,
            },
        ),
        SkillCommand::Export {
            skill_ref,
            team,
            out,
            force,
            dry_run,
            allow_yanked,
        } => registry_export::run(
            ctx,
            registry_export::Args {
                source: skill_ref,
                source_name: None,
                org: None,
                team,
                out: Some(out),
                force,
                dry_run,
                allow_yanked,
            },
        ),
        SkillCommand::Install {
            source,
            team,
            target,
            force,
            allow_yanked,
        } => install::run(
            ctx,
            install::Args {
                source: Some(source),
                source_name: None,
                org: None,
                team,
                target: Some(target),
                force,
                allow_yanked,
            },
        ),
        SkillCommand::Update {
            skill,
            target,
            check,
            force,
        } => update::run(
            ctx,
            update::Args {
                subject: Some(skill),
                subject_name: None,
                all: false,
                target: Some(target),
                check,
                force,
                prune: false,
            },
        ),
        SkillCommand::Uninstall {
            skill,
            target,
            force,
            yes,
            dry_run,
        } => uninstall::run(
            ctx,
            uninstall::Args {
                subject: skill,
                subject_name: None,
                target: Some(target),
                force,
                yes,
                dry_run,
            },
        ),
        SkillCommand::Visibility { action } => dispatch_skill_visibility(ctx, action),
        SkillCommand::Audit { skill_ref, team } => skill_audit(ctx, &skill_ref, team.as_deref()),
        SkillCommand::Version { action } => dispatch_skill_version(ctx, action),
    }
}

fn dispatch_skill_visibility(ctx: &Ctx, action: SkillVisibilityCommand) -> Result<()> {
    match action {
        SkillVisibilityCommand::Show { skill_ref, team } => {
            skill_visibility(ctx, &skill_ref, team.as_deref())
        }
        SkillVisibilityCommand::Set {
            skill_ref,
            scope,
            team,
        } => set_skill_visibility(ctx, &skill_ref, scope.as_visibility_str(), team.as_deref()),
    }
}

fn dispatch_skill_version(ctx: &Ctx, action: SkillVersionCommand) -> Result<()> {
    match action {
        SkillVersionCommand::List { skill_ref, team } => {
            versions::run(ctx, versions::Args { skill_ref, team })
        }
        SkillVersionCommand::Show { skill_ref, team } => {
            skill_version_show(ctx, &skill_ref, team.as_deref())
        }
        SkillVersionCommand::Approve { skill_ref, team } => approve::run(
            ctx,
            approve::Args {
                skill_ref,
                team,
                version: None,
            },
        ),
        SkillVersionCommand::Yank {
            skill_ref,
            team,
            reason,
        } => yank::run(
            ctx,
            yank::Args {
                skill_ref,
                team,
                reason,
            },
            yank::Action::Yank,
        ),
        SkillVersionCommand::Deprecate {
            skill_ref,
            team,
            reason,
        } => yank::run(
            ctx,
            yank::Args {
                skill_ref,
                team,
                reason,
            },
            yank::Action::Deprecate,
        ),
    }
}

fn dispatch_stack(ctx: &Ctx, action: StackCommand) -> Result<()> {
    match action {
        StackCommand::Create {
            stack_ref,
            scope,
            team,
            name,
            description,
        } => {
            let configured = configured_registry(ctx)?;
            let (org, stack) = refs::resolve_stack_ref(ctx, &configured.client, &stack_ref, None)?;
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::Create {
                    stack,
                    org,
                    visibility: scope.as_visibility_str().to_string(),
                    team,
                    name,
                    description,
                },
            )
        }
        StackCommand::List {
            org,
            team,
            owner,
            limit,
        } => {
            let org = match org {
                Some(org) => org,
                None => {
                    let configured = configured_registry(ctx)?;
                    refs::resolve_token_org(ctx, &configured.client, "stack list")?
                }
            };
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::List {
                    org,
                    team,
                    owner,
                    limit,
                },
            )
        }
        StackCommand::Show {
            stack_ref,
            team,
            target,
        } => {
            if let Some(target) = target {
                install_receipts::inspect(ctx, "stack", Some(&stack_ref), &target)
            } else {
                let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
                stack_registry::run(
                    ctx,
                    stack_registry::StackRegistryCommand::Inspect { stack, org },
                )
            }
        }
        StackCommand::Status { stack_ref, team } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            stack_status(ctx, &org, &stack)
        }
        StackCommand::Add {
            stack_ref,
            team,
            skill_ref,
            version_policy,
            pin_version,
        } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            let (skill, pin_version) = parse_stack_item_ref(&org, &skill_ref, pin_version)?;
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::Add {
                    stack,
                    skill,
                    org,
                    version_policy,
                    pin_version,
                },
            )
        }
        StackCommand::Remove {
            stack_ref,
            team,
            skill_ref,
            yes,
            dry_run,
        } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            let (skill, _) = parse_stack_item_ref(&org, &skill_ref, None)?;
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::Remove {
                    stack,
                    skill,
                    org,
                    yes,
                    dry_run,
                },
            )
        }
        StackCommand::Resolve { stack_ref, team } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::Resolve { stack, org },
            )
        }
        StackCommand::Export {
            stack_ref,
            team,
            out,
            force,
            dry_run,
        } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            registry_export::run(
                ctx,
                registry_export::Args {
                    source: "stack".to_string(),
                    source_name: Some(stack),
                    org: Some(org),
                    team: None,
                    out: Some(out),
                    force,
                    dry_run,
                    allow_yanked: false,
                },
            )
        }
        StackCommand::Install {
            stack_ref,
            team,
            target,
            force,
        } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            install::run(
                ctx,
                install::Args {
                    source: Some("stack".to_string()),
                    source_name: Some(stack),
                    org: Some(org),
                    team: None,
                    target: Some(target),
                    force,
                    allow_yanked: false,
                },
            )
        }
        StackCommand::Update {
            stack_ref,
            target,
            check,
            force,
            prune,
        } => update::run(
            ctx,
            update::Args {
                subject: Some("stack".to_string()),
                subject_name: Some(stack_ref),
                all: false,
                target: Some(target),
                check,
                force,
                prune,
            },
        ),
        StackCommand::Uninstall {
            stack_ref,
            target,
            force,
            yes,
            dry_run,
        } => uninstall::run(
            ctx,
            uninstall::Args {
                subject: "stack".to_string(),
                subject_name: Some(stack_ref),
                target: Some(target),
                force,
                yes,
                dry_run,
            },
        ),
        StackCommand::Visibility { action } => dispatch_stack_visibility(ctx, action),
        StackCommand::Audit { stack_ref, team } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            stack_audit(ctx, &org, &stack)
        }
    }
}

fn dispatch_stack_visibility(ctx: &Ctx, action: StackVisibilityCommand) -> Result<()> {
    match action {
        StackVisibilityCommand::Show { stack_ref, team } => {
            let (org, stack) = resolve_stack(ctx, &stack_ref, team.as_deref())?;
            stack_registry::run(
                ctx,
                stack_registry::StackRegistryCommand::Inspect { stack, org },
            )
        }
        StackVisibilityCommand::Set {
            stack_ref,
            scope,
            team,
        } => {
            let visibility: Visibility = scope.as_visibility_str().parse()?;
            validate_visibility_team_arg(visibility, team.as_deref())?;
            let configured = configured_registry(ctx)?;
            let (org, stack) = refs::resolve_stack_ref(ctx, &configured.client, &stack_ref, None)?;
            set_stack_visibility(ctx, &org, &stack, visibility, team.as_deref())
        }
    }
}

fn dispatch_install(ctx: &Ctx, action: InstallCommand) -> Result<()> {
    match action {
        InstallCommand::List { target, kind } => {
            install_receipts::list(ctx, &kind, target.as_deref())
        }
        InstallCommand::Why { skill, target } => install_receipts::why(ctx, &skill, &target),
        InstallCommand::Update {
            all,
            target,
            check,
            force,
        } => update::run(
            ctx,
            update::Args {
                subject: None,
                subject_name: None,
                all,
                target,
                check,
                force,
                prune: false,
            },
        ),
        InstallCommand::Doctor { target } => install_receipts::doctor(ctx, &target),
        InstallCommand::Unlock { target, force } => install_receipts::unlock(ctx, &target, force),
    }
}

fn dispatch_auth(ctx: &Ctx, action: AuthCommand) -> Result<()> {
    match action {
        AuthCommand::Login {
            token_stdin,
            provider,
            no_browser,
            callback_port,
            timeout_seconds,
        } => login::run(
            ctx,
            login::Args {
                token_stdin,
                provider: provider.as_str().to_string(),
                no_browser,
                callback_port,
                timeout_seconds,
            },
        ),
        AuthCommand::Status => whoami::run(ctx, whoami::Args { local: true }),
        AuthCommand::Logout => logout::run(ctx),
        AuthCommand::Whoami => whoami::run(ctx, whoami::Args { local: false }),
    }
}

fn dispatch_target(ctx: &Ctx, action: TargetCommand) -> Result<()> {
    match action {
        TargetCommand::List => targets::list(ctx),
        TargetCommand::Detect => targets::detect(ctx),
        TargetCommand::Setup { target, path, yes } => {
            setup::run(ctx, setup::Args { target, path, yes })
        }
        TargetCommand::Path { target } => targets::path(ctx, &target),
        TargetCommand::Set { target, path } => config::set_target(ctx, &target, path),
        TargetCommand::Unset { target } => config::unset_target(ctx, &target),
    }
}

fn dispatch_audit(ctx: &Ctx, action: AuditCommand) -> Result<()> {
    match action {
        AuditCommand::List { org } => {
            let org = resolve_optional_org(ctx, org.as_deref(), "audit list")?;
            org_audit(ctx, &org)
        }
        AuditCommand::Show { event_id, org } => {
            let org = resolve_optional_org(ctx, org.as_deref(), "audit show")?;
            org_audit_event(ctx, &org, &event_id)
        }
    }
}

fn resolve_optional_org(ctx: &Ctx, explicit_org: Option<&str>, context: &str) -> Result<String> {
    if let Some(org) = explicit_org {
        return Ok(org.to_string());
    }
    let configured = configured_registry(ctx)?;
    refs::resolve_token_org(ctx, &configured.client, context)
}

fn parse_stack_item_ref(
    stack_org: &str,
    raw: &str,
    pin_version: Option<String>,
) -> Result<(String, Option<String>)> {
    let mut parsed_pin = None;
    let skill = if raw.contains('/') {
        let skill_ref: SkillRef = raw.parse()?;
        if skill_ref.org != stack_org {
            bail!(
                "stack item `{raw}` is in org `{}`, but the stack is in org `{stack_org}`",
                skill_ref.org
            );
        }
        parsed_pin = skill_ref.version;
        skill_ref.name
    } else {
        let (name, inline_pin) = match raw.split_once('@') {
            Some((name, version)) => (name, Some(version.to_string())),
            None => (raw, None),
        };
        crate::skill::check_slug(name)
            .map_err(|reason| anyhow!("invalid skill `{name}`: {reason}"))?;
        if let Some(version) = inline_pin {
            crate::skill_ref::check_version(&version)?;
            parsed_pin = Some(version);
        }
        name.to_string()
    };

    match (parsed_pin, pin_version) {
        (Some(inline), Some(flag)) if inline != flag => {
            bail!("skill ref pins version `{inline}`, but --pin-version is `{flag}`")
        }
        (Some(inline), _) => Ok((skill, Some(inline))),
        (None, flag) => Ok((skill, flag)),
    }
}

fn configured_registry(ctx: &Ctx) -> Result<client::ConfiguredClient> {
    let configured = client::configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    Ok(configured)
}

/// Resolve a raw stack ref against the configured registry, returning
/// `(org, stack)`.
fn resolve_stack(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<(String, String)> {
    let configured = configured_registry(ctx)?;
    refs::resolve_stack_ref_with_team(ctx, &configured.client, raw, None, team)
}

/// Validate and resolve a raw skill ref against the configured registry,
/// returning the client alongside the resolved ref.
fn resolve_skill(
    ctx: &Ctx,
    raw: &str,
    team: Option<&str>,
) -> Result<(client::ConfiguredClient, SkillRef)> {
    refs::validate_skill_ref_input_with_team(raw, team)?;
    let configured = configured_registry(ctx)?;
    let skill_ref = refs::resolve_skill_ref_with_team(ctx, &configured.client, raw, team)?;
    Ok((configured, skill_ref))
}

fn skill_status(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let (configured, skill_ref) = resolve_skill(ctx, raw, team)?;
    let status = configured.client.skill_status(&skill_ref)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&skill_status_json(&status))?
        );
    } else if !ctx.quiet {
        println!("{}/{}", status.skill.org, status.skill.name);
        if let Some(owner_email) = &status.skill.owner_email {
            println!("  owner:            {owner_email}");
        }
        println!("  visibility:       {}", status.skill.visibility);
        println!(
            "  current version:  {}",
            status.skill.current_version.as_deref().unwrap_or("none")
        );
        println!(
            "  latest upload:    {}{}",
            status.skill.latest_version,
            version_state_suffix(&status.skill.latest_version, &status.versions)
        );
        println!("  uploaded versions: {}", status.versions.len());
        let next_command = skill_status_next_command(&status);
        let next = if skill_needs_approval(&status) {
            format!("org or team admin can approve with `{next_command}`")
        } else {
            next_command
        };
        println!("  next:             {next}");
    }
    Ok(())
}

fn skill_impact(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let (configured, skill_ref) = resolve_skill(ctx, raw, team)?;
    let impact = configured.client.skill_impact(&skill_ref)?;
    let next_commands = skill_impact_next_commands(&impact);
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&SkillImpactJson {
                impact: &impact,
                next_commands: &next_commands,
            })?
        );
    } else if !ctx.quiet {
        println!("skill: {}/{}", impact.skill.org, impact.skill.name);
        println!(
            "current version: {}",
            impact.skill.current_version.as_deref().unwrap_or("none")
        );
        println!(
            "latest upload:   {}{}",
            impact.skill.latest_version,
            skill_impact_latest_suffix(&impact)
        );
        println!("impacted stacks: {}", impact.summary.used_by_count);
        for stack in &impact.used_by {
            println!("  - {}", stack.stack);
        }
        println!("pinned references:");
        print_used_by_references(
            &impact,
            crate::registry::VersionPolicy::Pinned,
            "pins",
            "  none",
        );
        println!("unpinned/current references:");
        print_used_by_references(
            &impact,
            crate::registry::VersionPolicy::Current,
            "uses current",
            "  none",
        );
        println!("next:");
        for command in next_commands {
            println!("  {command}");
        }
    }
    Ok(())
}

fn skill_show(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let (configured, skill_ref) = resolve_skill(ctx, raw, team)?;
    let metadata = configured.client.skill_metadata(&skill_ref)?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&metadata)?);
    } else if !ctx.quiet {
        println!("{}", metadata.skill_ref());
        if let Some(owner_email) = metadata.owner_email {
            println!("  owner:      {owner_email}");
        }
        println!("  visibility: {}", metadata.visibility);
        println!(
            "  status:     {}",
            metadata
                .status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        println!("  current:    {}", metadata.current.unwrap_or(false));
        println!("  sha256:     {}", metadata.hash.hex);
        if let Some(created_at) = metadata.created_at {
            println!("  created:    {created_at}");
        }
        if let Some(install_count) = metadata.install_count {
            match metadata.last_installed_at {
                Some(last_installed_at) => {
                    println!("  installs:   {install_count} (last {last_installed_at})")
                }
                None => println!("  installs:   {install_count}"),
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct SkillImpactJson<'a> {
    #[serde(flatten)]
    impact: &'a SkillImpact,
    next_commands: &'a [String],
}

#[derive(Serialize)]
struct SkillStatusJson<'a> {
    skill: &'a crate::registry::RemoteSkill,
    versions: &'a [VersionInfo],
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<String>,
}

fn print_used_by_references(
    impact: &SkillImpact,
    policy: crate::registry::VersionPolicy,
    verb: &str,
    empty: &str,
) {
    let mut printed = false;
    for stack in impact
        .used_by
        .iter()
        .filter(|stack| stack.version_policy == policy)
    {
        printed = true;
        match (&stack.pinned_version, stack.effective_version.as_deref()) {
            (Some(pinned), _) => println!("  - {} {verb} v{pinned}", stack.stack),
            (None, Some(version)) => println!("  - {} {verb} v{version}", stack.stack),
            (None, None) => println!("  - {} {verb}", stack.stack),
        }
    }
    if !printed {
        println!("{empty}");
    }
}

fn skill_impact_latest_suffix(impact: &SkillImpact) -> &'static str {
    if impact.skill.current_version.as_deref() == Some(impact.skill.latest_version.as_str()) {
        " current"
    } else {
        " pending approval"
    }
}

fn skill_impact_next_commands(impact: &SkillImpact) -> Vec<String> {
    let mut commands = vec![format!(
        "agentstack skill status {}/{}",
        impact.skill.org, impact.skill.name
    )];
    if let Some(stack) = impact.used_by.first() {
        commands.push(format!("agentstack stack status {}", stack.stack));
    }
    commands
}

fn skill_version_show(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let input = refs::validate_skill_ref_input_with_team(raw, team)?;
    if input.version().is_none() {
        bail!("skill version show expects `skill@version` or `org/skill@version`");
    }
    skill_show(ctx, raw, team)
}

fn skill_visibility(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let (configured, skill_ref) = resolve_skill(ctx, raw, team)?;
    let status = configured.client.skill_visibility(&skill_ref)?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else if !ctx.quiet {
        println!("{}/{}", status.org, status.skill);
        println!("  visibility: {}", status.visibility);
        if let Some(team) = status.team {
            println!("  team:       {team}");
        }
    }
    Ok(())
}

fn set_skill_visibility(ctx: &Ctx, raw: &str, visibility: &str, team: Option<&str>) -> Result<()> {
    let visibility: Visibility = visibility.parse()?;
    validate_visibility_team_arg(visibility, team)?;
    refs::validate_skill_ref_input(raw)?;
    let configured = configured_registry(ctx)?;
    let skill_ref = refs::resolve_skill_ref(ctx, &configured.client, raw)?;
    let status = configured
        .client
        .set_skill_visibility(&skill_ref, visibility, team)?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else if !ctx.quiet {
        println!("visibility set {}/{}", status.org, status.skill);
        println!("  visibility: {}", status.visibility);
        if let Some(team) = status.team {
            println!("  team:       {team}");
        }
        if let Some(audit_event_id) = status.audit_event_id {
            println!("  audit_event_id: {audit_event_id}");
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct AuditEventsJson<'a> {
    events: &'a [AuditEvent],
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<String>,
}

#[derive(Serialize)]
struct StackStatusJson<'a> {
    stack: &'a crate::registry::StackDetail,
    next_command: String,
}

#[derive(Serialize)]
struct AuditEventJson<'a> {
    event: &'a AuditEvent,
}

fn skill_audit(ctx: &Ctx, raw: &str, team: Option<&str>) -> Result<()> {
    let (configured, skill_ref) = resolve_skill(ctx, raw, team)?;
    let events = configured.client.skill_audit(&skill_ref)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&audit_events_json(&events, Some(&skill_ref.org)))?
        );
    } else if !ctx.quiet {
        print_audit_events(&events, Some(&skill_ref.org));
    }
    Ok(())
}

fn stack_status(ctx: &Ctx, org: &str, stack: &str) -> Result<()> {
    let configured = configured_registry(ctx)?;
    let status = configured.client.stack_status(org, stack)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&stack_status_json(&status))?
        );
    } else if !ctx.quiet {
        println!("{}/{}", status.stack.org, status.stack.slug);
        if let Some(owner_email) = &status.stack.owner_email {
            println!("  owner:      {owner_email}");
        }
        println!("  visibility: {}", status.stack.visibility);
        println!("  items:      {}", status.stack.items.len());
        for item in &status.stack.items {
            let policy = item
                .pinned_version
                .as_ref()
                .map(|version| format!("{} @{version}", item.version_policy))
                .unwrap_or_else(|| item.version_policy.to_string());
            println!("  - {} ({policy})", item.skill);
        }
        println!(
            "  next:       agentstack stack resolve {}/{}",
            status.stack.org, status.stack.slug
        );
    }
    Ok(())
}

fn set_stack_visibility(
    ctx: &Ctx,
    org: &str,
    stack: &str,
    visibility: Visibility,
    team: Option<&str>,
) -> Result<()> {
    let configured = configured_registry(ctx)?;
    let detail = configured
        .client
        .set_stack_visibility(org, stack, visibility, team)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "stack": detail }))?
        );
    } else if !ctx.quiet {
        println!("visibility set {}/{}", detail.org, detail.slug);
        println!("  visibility: {}", detail.visibility);
        if let Some(team) = detail.team {
            println!("  team:       {team}");
        }
        if let Some(audit_event_id) = detail.audit_event_id {
            println!("  audit_event_id: {audit_event_id}");
        }
    }
    Ok(())
}

fn validate_visibility_team_arg(visibility: Visibility, team: Option<&str>) -> Result<()> {
    match (visibility, team) {
        (Visibility::Team, Some(team)) => {
            crate::skill::check_slug(team)
                .map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
            Ok(())
        }
        (Visibility::Team, None) => bail!("--team is required when --scope team is used"),
        (Visibility::Private | Visibility::Org, Some(_)) => {
            bail!("--team can only be used with --scope team")
        }
        (Visibility::Private | Visibility::Org, None) => Ok(()),
    }
}

fn stack_audit(ctx: &Ctx, org: &str, stack: &str) -> Result<()> {
    let configured = configured_registry(ctx)?;
    let events = configured.client.stack_audit(org, stack)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&audit_events_json(&events, Some(org)))?
        );
    } else if !ctx.quiet {
        print_audit_events(&events, Some(org));
    }
    Ok(())
}

fn org_audit(ctx: &Ctx, org: &str) -> Result<()> {
    crate::skill::check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
    let configured = configured_registry(ctx)?;
    let events = configured.client.org_audit(org)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&audit_events_json(&events, Some(org)))?
        );
    } else if !ctx.quiet {
        print_audit_events(&events, Some(org));
    }
    Ok(())
}

fn org_audit_event(ctx: &Ctx, org: &str, event_id: &str) -> Result<()> {
    crate::skill::check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
    let configured = configured_registry(ctx)?;
    let event = configured.client.org_audit_event(org, event_id)?;
    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&AuditEventJson { event: &event })?
        );
    } else if !ctx.quiet {
        print_audit_event(&event);
    }
    Ok(())
}

fn version_state_suffix(version: &str, versions: &[VersionInfo]) -> String {
    versions
        .iter()
        .find(|v| v.version == version)
        .map(|v| {
            // Lifecycle annotations take precedence over the raw status so this
            // matches `skill version list` (a yanked version reads "yanked",
            // not "approved").
            let state = if v.yanked_at.is_some() {
                "yanked".to_string()
            } else if v.deprecated_at.is_some() {
                "deprecated".to_string()
            } else {
                v.status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            };
            if v.current == Some(true) {
                format!(" ({state}, current)")
            } else {
                format!(" ({state})")
            }
        })
        .unwrap_or_default()
}

/// True when the latest upload still needs an approval to become current.
fn skill_needs_approval(status: &crate::registry::SkillStatus) -> bool {
    status.skill.current_version.as_deref() != Some(status.skill.latest_version.as_str())
}

fn skill_status_next_command(status: &crate::registry::SkillStatus) -> String {
    if skill_needs_approval(status) {
        format!(
            "agentstack skill version approve {}/{}@{}",
            status.skill.org, status.skill.name, status.skill.latest_version
        )
    } else {
        format!(
            "agentstack skill install {}/{} --target <target>",
            status.skill.org, status.skill.name
        )
    }
}

fn skill_status_json(status: &crate::registry::SkillStatus) -> SkillStatusJson<'_> {
    let next_command = skill_status_next_command(status);
    let concrete = crate::output::is_concrete_next_command(&next_command);
    SkillStatusJson {
        skill: &status.skill,
        versions: &status.versions,
        next_command: concrete.then(|| next_command.clone()),
        next_command_template: (!concrete).then_some(next_command),
    }
}

fn stack_status_json(status: &crate::registry::StackStatus) -> StackStatusJson<'_> {
    StackStatusJson {
        stack: &status.stack,
        next_command: format!(
            "agentstack stack resolve {}/{}",
            status.stack.org, status.stack.slug
        ),
    }
}

fn audit_events_json<'a>(events: &'a [AuditEvent], org: Option<&str>) -> AuditEventsJson<'a> {
    AuditEventsJson {
        events,
        next_command_template: org
            .filter(|_| !events.is_empty())
            .map(|org| format!("agentstack audit show <EVENT_ID> --org {org}")),
    }
}

fn print_audit_events(events: &[AuditEvent], org: Option<&str>) {
    if events.is_empty() {
        println!("no audit events");
        return;
    }
    println!("audit events: {}", events.len());
    println!(
        "  CREATED_AT            ACTION                    ACTOR                 RESOURCE          EVENT_ID"
    );
    for event in events {
        let actor = event.actor_email.as_deref().unwrap_or("-");
        let resource = event
            .resource
            .as_deref()
            .or(event.resource_id.as_deref())
            .unwrap_or("-");
        println!(
            "  {created:<20}  {action:<24}  {actor:<20}  {resource:<16}  {id}",
            created = event.created_at,
            action = event.action,
            actor = actor,
            resource = resource,
            id = event.id,
        );
    }
    if let Some(org) = org {
        println!("  next: agentstack audit show <EVENT_ID> --org {org}");
    }
}

fn print_audit_event(event: &AuditEvent) {
    println!("audit event {}", event.id);
    println!("  action:    {}", event.action);
    println!("  created:   {}", event.created_at);
    println!("  org:       {}", event.org);
    println!("  resource:  {}", event.resource.as_deref().unwrap_or("-"));
    if let Some(actor) = event.actor_email.as_deref() {
        println!("  actor:     {actor}");
    }
    if !event.metadata.is_null() {
        println!("  metadata:  {}", event.metadata);
    }
}

// Re-export the workflow-level entry points so integration tests can drive
// commands against a custom RegistryClient without launching the binary.
pub use adopt::{
    AdoptFailed, AdoptOptions, AdoptOutcome, AdoptSkipped, AdoptedSkill,
    run_with_client as adopt_with_client,
};
pub use approve::run_with_client as approve_with_client;
pub use candidates::{
    CandidateRow, CandidatesOptions, CandidatesReport, collect_candidates,
    run_with_client as candidates_with_client,
};
pub use diff::{
    DiffOptions, FileChangeSummary, InstalledDiffOptions,
    run_installed_with_client as diff_installed_with_client, run_with_client as diff_with_client,
};
pub use install::{
    RemoteInstallOptions, StackInstallOptions,
    run_remote_with_client as install_remote_with_client,
    run_stack_with_client as install_stack_with_client,
};
pub use list::run_remote_with_client as list_remote_with_client;
pub use push::{
    PushAllOptions, PushOptions, run_all_with_client as push_all_with_client,
    run_with_client as push_with_client,
};
pub use registry_export::{ExportOptions, run_with_client as registry_export_with_client};
pub use search::run_with_client as search_with_client;
pub use stack_registry::run_with_client as stack_registry_with_client;
pub use sync::{
    SyncAction, SyncEntry, SyncEntryKind, SyncEntryOutcome, SyncManifest, SyncOptions, SyncOutcome,
    load_manifest as load_sync_manifest, run_with_client as sync_with_client,
};
pub use teams::run_with_client as teams_with_client;
pub use update::{
    BatchUpdateRow, BatchUpdateRowStatus, StackUpdateOptions, UpdateAllOptions, UpdateOptions,
    run_all_with_client as update_all_with_client,
    run_stack_update_with_client as update_stack_with_client,
    run_with_client as update_with_client,
};
pub use versions::run_with_client as versions_with_client;
pub use yank::{Action as YankAction, YankOptions, run_with_client as yank_with_client};

#[cfg(test)]
mod tests {
    use super::version_state_suffix;
    use crate::package::PackageHash;
    use crate::registry::{VersionInfo, VersionStatus};

    fn version(v: &str) -> VersionInfo {
        VersionInfo {
            version: v.to_string(),
            hash: PackageHash::sha256_of(b"x"),
            platform_tags: Vec::new(),
            created_at: None,
            status: Some(VersionStatus::Approved),
            current: None,
            yanked_at: None,
            yank_reason: None,
            deprecated_at: None,
            deprecation_reason: None,
        }
    }

    #[test]
    fn version_state_suffix_reflects_lifecycle_over_raw_status() {
        // A yanked version reads "yanked", not "approved" (matches version list).
        let mut yanked = version("1");
        yanked.yanked_at = Some("2026-01-01T00:00:00Z".to_string());
        assert_eq!(version_state_suffix("1", &[yanked]), " (yanked)");

        // A deprecated current version stays current and reads "deprecated".
        let mut deprecated = version("1");
        deprecated.deprecated_at = Some("2026-01-01T00:00:00Z".to_string());
        deprecated.current = Some(true);
        assert_eq!(
            version_state_suffix("1", &[deprecated]),
            " (deprecated, current)"
        );

        // A plain approved current version is unchanged.
        let mut approved = version("1");
        approved.current = Some(true);
        assert_eq!(
            version_state_suffix("1", &[approved]),
            " (approved, current)"
        );
    }
}
