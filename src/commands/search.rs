//! `agentstack skill search` — query the active registry for skills.

use anyhow::{Context, Result};
use serde::Serialize;

use super::client::{configured_client, registry_context};
use crate::output::{Ctx, compact_human_text};
use crate::registry::{CatalogSort, RegistryClient, SearchFilters, SearchResult, Visibility};
use crate::skill::check_slug;

const DESCRIPTION_MAX_CHARS: usize = 96;

pub struct Args {
    pub query: String,
    pub org: Option<String>,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub visibility: Option<String>,
    pub owner: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<usize>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let filters = filters_from_args(&args)?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    run_with_client(
        &configured.client,
        Some(&configured.url),
        &args.query,
        &filters,
        ctx.json,
        ctx.quiet,
    )
}

fn filters_from_args(args: &Args) -> Result<SearchFilters> {
    if let Some(org) = args.org.as_deref() {
        check_slug(org).map_err(|reason| anyhow::anyhow!("invalid --org `{org}`: {reason}"))?;
    }
    if let Some(team) = args.team.as_deref() {
        check_slug(team).map_err(|reason| anyhow::anyhow!("invalid --team `{team}`: {reason}"))?;
    }
    let visibility = args
        .visibility
        .as_deref()
        .map(str::parse::<Visibility>)
        .transpose()
        .context("invalid --visibility")?;
    let sort = args
        .sort
        .as_deref()
        .map(str::parse::<CatalogSort>)
        .transpose()
        .context("invalid --sort")?;
    Ok(SearchFilters {
        org: args.org.clone(),
        team: args.team.clone(),
        platforms: args.platforms.clone(),
        visibility,
        owner: args.owner.clone(),
        sort,
        limit: args.limit,
    })
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    query: &str,
    filters: &SearchFilters,
    json: bool,
    quiet: bool,
) -> Result<()> {
    let results = client
        .search_with_filters(query, filters)
        .with_context(|| registry_context(registry_url, "search on", "search"))?;

    if json {
        println!("{}", render_json(query, filters, &results)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("{}", empty_message(query, filters));
        if !quiet {
            println!("next: {}", discovery_suggestion(query, filters));
        }
        return Ok(());
    }

    print_summary("matches", results.len(), filters, quiet);
    let show_installs = results.iter().any(|r| r.install_count.is_some());
    let rows: Vec<_> = results
        .iter()
        .map(|r| {
            let skill = format!("{}/{}", r.org, r.name);
            let current = current_label(r.current_version.as_deref());
            let latest = latest_label(&r.latest_version, r.current_version.as_deref());
            (
                skill,
                current,
                latest,
                r.visibility.to_string(),
                owner_label(r.owner_email.as_deref()),
                installs_label(r.install_count),
                &r.description,
            )
        })
        .collect();
    let skill_width = rows
        .iter()
        .map(|(skill, _, _, _, _, _, _)| skill.len())
        .max()
        .unwrap_or(0)
        .max("SKILL".len());
    let current_width = rows
        .iter()
        .map(|(_, current, _, _, _, _, _)| current.len())
        .max()
        .unwrap_or(0)
        .max("CURRENT".len());
    let latest_width = rows
        .iter()
        .map(|(_, _, latest, _, _, _, _)| latest.len())
        .max()
        .unwrap_or(0)
        .max("LATEST".len());
    let visibility_width = rows
        .iter()
        .map(|(_, _, _, visibility, _, _, _)| visibility.len())
        .max()
        .unwrap_or(0)
        .max("VISIBILITY".len());
    let owner_width = rows
        .iter()
        .map(|(_, _, _, _, owner, _, _)| owner.len())
        .max()
        .unwrap_or(0)
        .max("OWNER".len());
    let installs_width = rows
        .iter()
        .map(|(_, _, _, _, _, installs, _)| installs.len())
        .max()
        .unwrap_or(0)
        .max("INSTALLS".len());

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
    for (skill, current, latest, visibility, owner, installs, description) in rows {
        let mut line = format!(
            "{skill:<sw$}  {current:<cw$}  {latest:<lw$}  {visibility:<vw$}  {owner:<ow$}",
            sw = skill_width,
            cw = current_width,
            lw = latest_width,
            vw = visibility_width,
            ow = owner_width,
        );
        if show_installs {
            line.push_str(&format!("  {installs:<iw$}", iw = installs_width));
        }
        println!("{line}");
        println!(
            "  description: {}",
            compact_human_text(description, DESCRIPTION_MAX_CHARS)
        );
    }
    Ok(())
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

fn empty_message(query: &str, filters: &SearchFilters) -> String {
    let mut message = format!("no skills matched `{query}`");
    let mut qualifiers = Vec::new();
    if let Some(org) = filters.org.as_deref() {
        qualifiers.push(format!("org `{org}`"));
    }
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
        message.push_str(" in ");
        message.push_str(&qualifiers.join(", "));
    }
    message.push('.');
    message
}

fn discovery_suggestion(query: &str, filters: &SearchFilters) -> String {
    let has_narrowing_filter = filters.team.is_some()
        || !filters.platforms.is_empty()
        || filters.visibility.is_some()
        || filters.owner.is_some();

    if has_narrowing_filter {
        let mut command = format!("agentstack skill search {}", shell_word(query));
        if let Some(org) = filters.org.as_deref() {
            command.push_str(&format!(" --org {org}"));
        }
        return command;
    }

    let mut command = "agentstack skill list".to_string();
    if let Some(org) = filters.org.as_deref() {
        command.push_str(&format!(" --org {org}"));
    }
    command
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    query: &'a str,
    filters: JsonFilters<'a>,
    results: &'a [SearchResult],
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
}

#[derive(Serialize)]
struct JsonFilters<'a> {
    org: Option<&'a str>,
    team: Option<&'a str>,
    platform: &'a [String],
    visibility: Option<&'a str>,
    owner: Option<&'a str>,
    sort: Option<&'a str>,
    limit: Option<usize>,
}

fn render_json(query: &str, filters: &SearchFilters, results: &[SearchResult]) -> Result<String> {
    let visibility = filters.visibility.map(|v| v.as_str());
    let out = JsonOutput {
        query,
        filters: JsonFilters {
            org: filters.org.as_deref(),
            team: filters.team.as_deref(),
            platform: &filters.platforms,
            visibility,
            owner: filters.owner.as_deref(),
            sort: filters.sort.map(|sort| sort.as_str()),
            limit: filters.limit,
        },
        results,
        empty_message: results.is_empty().then(|| empty_message(query, filters)),
        next_command: results
            .is_empty()
            .then(|| discovery_suggestion(query, filters))
            .filter(|command| crate::output::is_concrete_next_command(command)),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}
