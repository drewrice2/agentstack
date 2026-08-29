//! `agentstack team ...` — team and team-membership management.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use super::client::configured_client;
use super::refs;
use crate::cli::TeamsCommand;
use crate::output::Ctx;
use crate::registry::{RegistryClient, TeamDetail, TeamSummary};
use crate::skill::check_slug;

pub fn run(ctx: &Ctx, action: TeamsCommand) -> Result<()> {
    validate_action(&action)?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let action = match action {
        TeamsCommand::List { org: None } => TeamsCommand::List {
            org: Some(refs::resolve_token_org(
                ctx,
                &configured.client,
                "team list",
            )?),
        },
        other => other,
    };
    run_with_client(&configured.client, action, ctx.json, ctx.quiet)
}

fn validate_action(action: &TeamsCommand) -> Result<()> {
    match action {
        TeamsCommand::Create { team_ref }
        | TeamsCommand::Inspect { team_ref }
        | TeamsCommand::RemoveMember { team_ref, .. } => {
            parse_team_ref(team_ref)?;
        }
        TeamsCommand::AddMember { team_ref, role, .. }
        | TeamsCommand::SetRole { team_ref, role, .. } => {
            parse_team_ref(team_ref)?;
            parse_role(role)?;
        }
        TeamsCommand::List { org } => {
            if let Some(org) = org {
                check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
            }
        }
    }
    Ok(())
}

fn parse_team_ref(raw: &str) -> Result<(String, String)> {
    let (org, team) = raw
        .split_once('/')
        .ok_or_else(|| anyhow!("team ref must be `org/team`, got `{raw}`"))?;
    if org.is_empty() || team.is_empty() {
        bail!("team ref must be `org/team`, got `{raw}`");
    }
    check_slug(org).map_err(|reason| anyhow!("invalid org `{org}`: {reason}"))?;
    check_slug(team).map_err(|reason| anyhow!("invalid team `{team}`: {reason}"))?;
    Ok((org.to_string(), team.to_string()))
}

fn parse_role(raw: &str) -> Result<&'static str> {
    match raw {
        "member" => Ok("member"),
        "team_admin" => Ok("team_admin"),
        other => bail!("unknown role `{other}` (expected one of: member, team_admin)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_role_returns_canonical_roles() {
        assert_eq!(parse_role("member").unwrap(), "member");
        assert_eq!(parse_role("team_admin").unwrap(), "team_admin");

        let err = parse_role("lead").unwrap_err().to_string();
        assert!(err.contains("member, team_admin"), "err: {err}");
        assert!(!err.contains("legacy lead"), "err: {err}");
        let err = parse_role("manager").unwrap_err().to_string();
        assert!(err.contains("member, team_admin"), "err: {err}");
    }
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    action: TeamsCommand,
    json: bool,
    quiet: bool,
) -> Result<()> {
    match action {
        TeamsCommand::Create { team_ref } => {
            let (org, team) = parse_team_ref(&team_ref)?;
            let detail = client
                .create_team(&org, &team)
                .with_context(|| format!("create team {org}/{team} failed"))?;
            print_detail(&detail, json, quiet, "created")?;
        }
        TeamsCommand::List { org } => {
            let org = org.expect("team list org is resolved before run_with_client");
            let mut teams = client
                .list_teams(&org)
                .with_context(|| format!("list teams in {org} failed"))?;
            teams.sort_by(|a, b| a.slug.cmp(&b.slug));
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&TeamsList {
                        teams: &teams,
                        empty_message: teams.is_empty().then(|| team_list_empty_message(&org)),
                        next_command_template: teams
                            .is_empty()
                            .then(|| team_list_next_command_template(&org)),
                    })?
                );
            } else if !quiet {
                if teams.is_empty() {
                    println!("{}", team_list_empty_message(&org));
                    println!("next: agentstack team create {org}/<team>");
                } else {
                    for t in &teams {
                        println!("{}/{}", t.org, t.slug);
                    }
                }
            }
        }
        TeamsCommand::Inspect { team_ref } => {
            let (org, team) = parse_team_ref(&team_ref)?;
            let detail = client
                .inspect_team(&org, &team)
                .with_context(|| format!("inspect team {org}/{team} failed"))?;
            print_detail(&detail, json, quiet, "team")?;
        }
        TeamsCommand::AddMember {
            team_ref,
            email,
            role,
        } => {
            let (org, team) = parse_team_ref(&team_ref)?;
            let role = parse_role(&role)?;
            let detail = client
                .add_team_member(&org, &team, &email, role)
                .with_context(|| format!("add member to {org}/{team} failed"))?;
            print_detail(&detail, json, quiet, "team")?;
        }
        TeamsCommand::RemoveMember { team_ref, email } => {
            let (org, team) = parse_team_ref(&team_ref)?;
            let detail = client
                .remove_team_member(&org, &team, &email)
                .with_context(|| format!("remove member from {org}/{team} failed"))?;
            print_detail(&detail, json, quiet, "team")?;
        }
        TeamsCommand::SetRole {
            team_ref,
            email,
            role,
        } => {
            let (org, team) = parse_team_ref(&team_ref)?;
            let role = parse_role(&role)?;
            let detail = client
                .set_team_role(&org, &team, &email, role)
                .with_context(|| format!("set role on {org}/{team} failed"))?;
            print_detail(&detail, json, quiet, "team")?;
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct TeamsList<'a> {
    teams: &'a [TeamSummary],
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<String>,
}

fn team_list_empty_message(org: &str) -> String {
    format!("no teams found in `{org}`.")
}

fn team_list_next_command_template(org: &str) -> String {
    format!("agentstack team create {org}/<team>")
}

#[derive(Serialize)]
struct TeamEnvelope<'a> {
    team: &'a TeamDetail,
    #[serde(skip_serializing_if = "Option::is_none")]
    audit_event_id: Option<&'a str>,
}

fn print_detail(detail: &TeamDetail, json: bool, quiet: bool, label: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&TeamEnvelope {
                team: detail,
                audit_event_id: detail.audit_event_id.as_deref(),
            })?
        );
    } else if !quiet {
        println!("{label} {}/{}", detail.org, detail.slug);
        if let Some(audit_event_id) = &detail.audit_event_id {
            println!("  audit_event_id: {audit_event_id}");
        }
        for m in &detail.members {
            println!("  {} ({})", m.email, m.role);
        }
    }
    Ok(())
}
