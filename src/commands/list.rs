use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::{configured_client, registry_context};
use crate::output::{Ctx, compact_human_text, is_concrete_next_command};
use crate::registry::{CatalogSort, RegistryClient, RemoteSkill, SearchFilters};
use crate::skill::check_slug;
use crate::skill::discover_skills;

const DESCRIPTION_MAX_CHARS: usize = 96;

pub struct Args {
    pub local: bool,
    pub remote: bool,
    pub org: Option<String>,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub visibility: Option<String>,
    pub owner: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<usize>,
    pub path: Option<PathBuf>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.remote {
        return run_remote(ctx, &args);
    }
    if !args.local {
        bail!(
            "specify --local to scan the current directory or --remote to query the active registry"
        );
    }
    if args.org.is_some() {
        bail!("--org only applies with --remote");
    }
    if args.team.is_some() {
        bail!("--team only applies with --remote");
    }
    run_local(ctx, args.path.as_deref())
}

fn run_remote(ctx: &Ctx, args: &Args) -> Result<()> {
    let org = args.org.as_deref();
    let team = args.team.as_deref();
    if let Some(org) = org {
        check_slug(org).map_err(|reason| anyhow::anyhow!("invalid --org `{org}`: {reason}"))?;
    }
    if let Some(team) = team {
        check_slug(team).map_err(|reason| anyhow::anyhow!("invalid --team `{team}`: {reason}"))?;
    }
    let visibility = args
        .visibility
        .as_deref()
        .map(str::parse)
        .transpose()
        .context("invalid --scope")?;
    let sort = args
        .sort
        .as_deref()
        .map(str::parse::<CatalogSort>)
        .transpose()
        .context("invalid --sort")?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let filters = SearchFilters {
        org: org.map(str::to_string),
        team: team.map(str::to_string),
        platforms: args.platforms.clone(),
        visibility,
        owner: args.owner.clone(),
        sort,
        limit: args.limit,
    };
    run_remote_with_client(
        &configured.client,
        Some(&configured.url),
        &filters,
        ctx.json,
        ctx.quiet,
    )
}

pub fn run_remote_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    filters: &SearchFilters,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let skills = client
        .list_remote_with_filters(filters)
        .with_context(|| registry_context(registry_url, "list on", "list"))?;

    if json {
        println!("{}", render_remote_json(filters, &skills)?);
        return Ok(());
    }

    if skills.is_empty() {
        println!("{}", empty_message(filters));
        if !quiet {
            println!("next: {}", discovery_suggestion(filters));
        }
        return Ok(());
    }

    print_summary("skills", skills.len(), filters, quiet);
    let show_installs = skills.iter().any(|r| r.install_count.is_some());
    let rows: Vec<_> = skills
        .iter()
        .map(|r| RemoteRow {
            skill: format!("{}/{}", r.org, r.name),
            current: current_label(r.current_version.as_deref()),
            latest: latest_label(&r.latest_version, r.current_version.as_deref()),
            visibility: r.visibility.to_string(),
            owner: owner_label(r.owner_email.as_deref()),
            installs: installs_label(r.install_count),
            description: &r.description,
        })
        .collect();
    let skill_width = column_width(&rows, "SKILL", |r| r.skill.len());
    let current_width = column_width(&rows, "CURRENT", |r| r.current.len());
    let latest_width = column_width(&rows, "LATEST", |r| r.latest.len());
    let visibility_width = column_width(&rows, "VISIBILITY", |r| r.visibility.len());
    let owner_width = column_width(&rows, "OWNER", |r| r.owner.len());
    let installs_width = column_width(&rows, "INSTALLS", |r| r.installs.len());

    let mut header = format!(
        "{:<sw$}  {:<cw$}  {:<lw$}  {:<vw$}  {:<ow$}",
        "SKILL",
        "CURRENT",
        "LATEST",
        "VISIBILITY",
        "OWNER",
        sw = skill_width,
        cw = current_width,
        lw = latest_width,
        vw = visibility_width,
        ow = owner_width,
    );
    if show_installs {
        header.push_str(&format!("  {:<iw$}", "INSTALLS", iw = installs_width));
    }
    println!("{header}");
    for row in rows {
        let mut line = format!(
            "{skill:<sw$}  {current:<cw$}  {latest:<lw$}  {visibility:<vw$}  {owner:<ow$}",
            skill = row.skill,
            current = row.current,
            latest = row.latest,
            visibility = row.visibility,
            owner = row.owner,
            sw = skill_width,
            cw = current_width,
            lw = latest_width,
            vw = visibility_width,
            ow = owner_width,
        );
        if show_installs {
            line.push_str(&format!("  {:<iw$}", row.installs, iw = installs_width));
        }
        println!("{line}");
        println!(
            "  description: {}",
            compact_human_text(row.description, DESCRIPTION_MAX_CHARS)
        );
    }
    Ok(())
}

struct RemoteRow<'a> {
    skill: String,
    current: String,
    latest: String,
    visibility: String,
    owner: String,
    installs: String,
    description: &'a str,
}

fn column_width(
    rows: &[RemoteRow<'_>],
    header: &str,
    len: impl Fn(&RemoteRow<'_>) -> usize,
) -> usize {
    rows.iter().map(len).max().unwrap_or(0).max(header.len())
}

fn owner_label(owner: Option<&str>) -> String {
    owner.unwrap_or("-").to_string()
}

fn installs_label(install_count: Option<u64>) -> String {
    install_count
        .map(|count| count.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn current_label(current: Option<&str>) -> String {
    current
        .map(|version| format!("v{version} approved"))
        .unwrap_or_else(|| "-".to_string())
}

fn latest_label(latest: &str, current: Option<&str>) -> String {
    if current == Some(latest) {
        format!("v{latest} approved")
    } else {
        format!("v{latest} not current")
    }
}

fn print_summary(noun: &str, count: usize, filters: &SearchFilters, quiet: bool) {
    if quiet {
        return;
    }
    let mut parts = Vec::new();
    if let Some(org) = filters.org.as_deref() {
        parts.push(format!("org={org}"));
    }
    if let Some(team) = filters.team.as_deref() {
        parts.push(format!("team={team}"));
    }
    if !filters.platforms.is_empty() {
        parts.push(format!("platform={}", filters.platforms.join(",")));
    }
    if let Some(visibility) = filters.visibility {
        parts.push(format!("scope={visibility}"));
    }
    if let Some(owner) = filters.owner.as_deref() {
        parts.push(format!("owner={owner}"));
    }
    if let Some(sort) = filters.sort {
        parts.push(format!("sort={}", sort.as_str()));
    }
    if let Some(limit) = filters.limit {
        parts.push(format!("limit={limit}"));
    }
    if parts.is_empty() {
        println!("showing {count} {noun}.");
    } else {
        println!("showing {count} {noun} ({})", parts.join(", "));
    }
}

fn empty_message(filters: &SearchFilters) -> String {
    let mut message = match filters.org.as_deref() {
        Some(org) => format!("no skills found in `{org}`"),
        None => "no skills found".to_string(),
    };
    let mut qualifiers = Vec::new();
    if let Some(team) = filters.team.as_deref() {
        qualifiers.push(format!("team `{team}`"));
    }
    if !filters.platforms.is_empty() {
        qualifiers.push(format!("platform `{}`", filters.platforms.join(",")));
    }
    if let Some(visibility) = filters.visibility {
        qualifiers.push(format!("scope `{visibility}`"));
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

fn discovery_suggestion(filters: &SearchFilters) -> String {
    let has_narrowing_filter = filters.team.is_some()
        || !filters.platforms.is_empty()
        || filters.visibility.is_some()
        || filters.owner.is_some();

    if has_narrowing_filter {
        let mut command = "agentstack skill list".to_string();
        if let Some(org) = filters.org.as_deref() {
            command.push_str(&format!(" --org {org}"));
        }
        return command;
    }

    let mut command = "agentstack skill search <query>".to_string();
    if let Some(org) = filters.org.as_deref() {
        command.push_str(&format!(" --org {org}"));
    }
    command
}

fn run_local(ctx: &Ctx, path: Option<&Path>) -> Result<()> {
    let root = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to read current directory")?,
    };
    let mut found: Vec<(String, String)> = Vec::new();

    for skill in discover_skills(&root)? {
        match (skill.validation.is_ok(), skill.validation.manifest()) {
            (true, Some(manifest)) => found.push((manifest.name, manifest.description)),
            _ => {
                let first = skill.validation.errors.first();
                let msg = first
                    .map(|e| e.message.as_str())
                    .unwrap_or("invalid skill directory");
                if !ctx.json && !ctx.quiet {
                    ctx.warn(format!(
                        "warning: skipping `{}`: {msg}",
                        skill.path.display()
                    ));
                }
            }
        }
    }

    found.sort_by(|a, b| a.0.cmp(&b.0));

    if ctx.json {
        ctx.say_always(render_local_json(&root, &found)?);
        return Ok(());
    }

    if found.is_empty() {
        ctx.say(local_empty_message(&root));
        ctx.say(format!("next: {}", local_next_command()));
        return Ok(());
    }

    let name_width = found.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, description) in &found {
        ctx.say_always(format!("{name:<width$}  {description}", width = name_width));
    }
    Ok(())
}

#[derive(Serialize)]
struct LocalJson<'a> {
    skills: Vec<LocalSkillRow<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
}

#[derive(Serialize)]
struct LocalSkillRow<'a> {
    name: &'a str,
    description: &'a str,
}

fn render_local_json(root: &Path, found: &[(String, String)]) -> Result<String> {
    let out = LocalJson {
        skills: found
            .iter()
            .map(|(n, d)| LocalSkillRow {
                name: n,
                description: d,
            })
            .collect(),
        empty_message: found.is_empty().then(|| local_empty_message(root)),
        next_command: found.is_empty().then(local_next_command),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn local_empty_message(root: &Path) -> String {
    format!("no skills found under {}", root.display())
}

fn local_next_command() -> &'static str {
    "agentstack skill init my-skill --name my-skill --description \"Use when reviewing PRs\""
}

#[derive(Serialize)]
struct RemoteJson<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    org: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<&'a str>,
    filters: RemoteJsonFilters<'a>,
    skills: &'a [RemoteSkill],
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<String>,
}

#[derive(Serialize)]
struct RemoteJsonFilters<'a> {
    org: Option<&'a str>,
    team: Option<&'a str>,
    platform: &'a [String],
    visibility: Option<&'a str>,
    owner: Option<&'a str>,
    sort: Option<&'a str>,
    limit: Option<usize>,
}

fn render_remote_json(filters: &SearchFilters, skills: &[RemoteSkill]) -> Result<String> {
    let visibility = filters.visibility.map(|v| v.as_str());
    let next_command = skills.is_empty().then(|| discovery_suggestion(filters));
    let out = RemoteJson {
        org: filters.org.as_deref(),
        team: filters.team.as_deref(),
        filters: RemoteJsonFilters {
            org: filters.org.as_deref(),
            team: filters.team.as_deref(),
            platform: &filters.platforms,
            visibility,
            owner: filters.owner.as_deref(),
            sort: filters.sort.map(|sort| sort.as_str()),
            limit: filters.limit,
        },
        skills,
        empty_message: skills.is_empty().then(|| empty_message(filters)),
        next_command: next_command
            .as_ref()
            .filter(|command| is_concrete_next_command(command))
            .cloned(),
        next_command_template: next_command.filter(|command| !is_concrete_next_command(command)),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}
