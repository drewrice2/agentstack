//! `agentstack skill candidates` — approval inbox listing pending candidate
//! versions across visible skills.
//!
//! The registry has no bulk-candidates endpoint, so this aggregates
//! client-side: list visible skills for one org, fetch each skill's version
//! list, and keep versions whose status is `candidate` and that are not
//! yanked.

use anyhow::{Context, Result, anyhow};
use serde::Serialize;

use super::{client::configured_client, refs};
use crate::output::Ctx;
use crate::registry::{RegistryClient, SearchFilters, VersionStatus};
use crate::skill::check_slug;
use crate::skill_ref::SkillRef;

pub struct Args {
    pub org: Option<String>,
    pub limit: usize,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if let Some(org) = args.org.as_deref() {
        check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
    }
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let org = match args.org {
        Some(org) => org,
        None => refs::resolve_token_org(ctx, &configured.client, "skill candidates")?,
    };
    run_with_client(
        &configured.client,
        Some(&configured.url),
        &CandidatesOptions {
            org: &org,
            limit: args.limit,
            json: ctx.json,
            quiet: ctx.quiet,
            verbose: ctx.verbose,
        },
    )
}

pub struct CandidatesOptions<'a> {
    pub org: &'a str,
    pub limit: usize,
    pub json: bool,
    pub quiet: bool,
    pub verbose: bool,
}

/// One pending candidate version in the approval inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CandidateRow {
    pub org: String,
    pub skill: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub approve_command: String,
}

/// One skill whose version list could not be read during the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSkill {
    pub skill_ref: String,
    pub error: String,
}

/// Aggregated scan result. `truncated` is true when more visible skills
/// exist beyond the scan limit.
#[derive(Debug)]
pub struct CandidatesReport {
    pub candidates: Vec<CandidateRow>,
    pub scanned_skills: usize,
    pub truncated: bool,
    pub skipped: Vec<SkippedSkill>,
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    options: &CandidatesOptions<'_>,
) -> Result<()> {
    let report =
        collect_candidates(client, options.org, options.limit).with_context(
            || match registry_url {
                Some(url) => format!("candidates scan on {url} failed"),
                None => "candidates scan failed".to_string(),
            },
        )?;

    if options.verbose && !options.json {
        for skipped in &report.skipped {
            eprintln!(
                "[verbose] skipped `{}`: {}",
                skipped.skill_ref, skipped.error
            );
        }
    }

    if options.json {
        println!("{}", render_json(options.org, &report)?);
        return Ok(());
    }

    if report.candidates.is_empty() {
        println!("{}", empty_message(options.org));
        if !options.quiet {
            println!("next: {}", empty_next_command(options.org));
        }
        print_truncation_note(&report, options);
        return Ok(());
    }

    if !options.quiet {
        println!(
            "{} candidate version{} awaiting approval (org={}, scanned {} skill{}).",
            report.candidates.len(),
            plural(report.candidates.len()),
            options.org,
            report.scanned_skills,
            plural(report.scanned_skills),
        );
        println!();
    }

    let rows: Vec<_> = report
        .candidates
        .iter()
        .map(|row| {
            (
                format!("{}/{}", row.org, row.skill),
                format!("v{}", row.version),
                row.created_at.as_deref().unwrap_or("-").to_string(),
                row.owner.as_deref().unwrap_or("-").to_string(),
            )
        })
        .collect();
    let skill_width = rows
        .iter()
        .map(|(skill, _, _, _)| skill.len())
        .max()
        .unwrap_or(0)
        .max("SKILL".len());
    let version_width = rows
        .iter()
        .map(|(_, version, _, _)| version.len())
        .max()
        .unwrap_or(0)
        .max("VERSION".len());
    let created_width = rows
        .iter()
        .map(|(_, _, created, _)| created.len())
        .max()
        .unwrap_or(0)
        .max("CREATED".len());
    println!(
        "{:<sw$}  {:<vw$}  {:<cw$}  OWNER",
        "SKILL",
        "VERSION",
        "CREATED",
        sw = skill_width,
        vw = version_width,
        cw = created_width,
    );
    for (skill, version, created, owner) in &rows {
        println!(
            "{skill:<sw$}  {version:<vw$}  {created:<cw$}  {owner}",
            sw = skill_width,
            vw = version_width,
            cw = created_width,
        );
    }

    if !options.quiet {
        println!();
        println!("next:");
        for row in &report.candidates {
            println!("  {}", row.approve_command);
        }
    }
    print_truncation_note(&report, options);
    Ok(())
}

/// Pure aggregation step, separated from rendering so tests and other
/// commands can reuse it.
pub fn collect_candidates(
    client: &dyn RegistryClient,
    org: &str,
    limit: usize,
) -> Result<CandidatesReport> {
    let filters = SearchFilters {
        org: Some(org.to_string()),
        // Ask for one extra row so truncation is detected without a second
        // request, and re-truncate locally in case the server ignores limits.
        limit: Some(limit.saturating_add(1)),
        ..SearchFilters::default()
    };
    let mut skills = client.list_remote_with_filters(&filters)?;
    let truncated = skills.len() > limit;
    skills.truncate(limit);
    let scanned_skills = skills.len();

    let mut candidates = Vec::new();
    let mut skipped = Vec::new();
    for skill in &skills {
        let skill_ref = match SkillRef::new(skill.org.clone(), skill.name.clone()) {
            Ok(skill_ref) => skill_ref,
            Err(err) => {
                skipped.push(SkippedSkill {
                    skill_ref: skill.skill_ref(),
                    error: err.to_string(),
                });
                continue;
            }
        };
        let versions = match client.list_versions(&skill_ref) {
            Ok(versions) => versions,
            Err(err) => {
                skipped.push(SkippedSkill {
                    skill_ref: skill.skill_ref(),
                    error: format!("{err:#}"),
                });
                continue;
            }
        };
        for version in &versions {
            if version.status != Some(VersionStatus::Candidate) || version.yanked_at.is_some() {
                continue;
            }
            candidates.push(CandidateRow {
                org: skill.org.clone(),
                skill: skill.name.clone(),
                version: version.version.clone(),
                created_at: version.created_at.clone(),
                owner: skill.owner_email.clone(),
                approve_command: format!(
                    "agentstack skill version approve {}/{}@{}",
                    skill.org, skill.name, version.version
                ),
            });
        }
    }

    // Newest first; rows without a created timestamp sort last. Ties fall
    // back to skill ref and then highest version for a stable order.
    candidates.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.org.cmp(&b.org))
            .then_with(|| a.skill.cmp(&b.skill))
            .then_with(|| b.version.cmp(&a.version))
    });

    Ok(CandidatesReport {
        candidates,
        scanned_skills,
        truncated,
        skipped,
    })
}

fn print_truncation_note(report: &CandidatesReport, options: &CandidatesOptions<'_>) {
    if report.truncated && !options.quiet {
        eprintln!(
            "note: scanned only the first {} visible skill{}; rerun with a higher --limit to scan more.",
            options.limit,
            plural(options.limit),
        );
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    candidates: &'a [CandidateRow],
    scanned_skills: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
}

fn render_json(org: &str, report: &CandidatesReport) -> Result<String> {
    let out = JsonOutput {
        candidates: &report.candidates,
        scanned_skills: report.scanned_skills,
        truncated: report.truncated,
        empty_message: report.candidates.is_empty().then(|| empty_message(org)),
        next_command: report
            .candidates
            .is_empty()
            .then(|| empty_next_command(org)),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn empty_message(org: &str) -> String {
    format!("no candidate versions awaiting approval in `{org}`.")
}

fn empty_next_command(org: &str) -> String {
    format!("agentstack skill list --org {org}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::PackageHash;
    use crate::registry::{MockRegistryClient, SkillMetadata, Visibility};

    fn metadata(
        org: &str,
        name: &str,
        version: &str,
        status: VersionStatus,
        created_at: &str,
    ) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: format!("Use when working on {name}"),
            org: org.to_string(),
            owner_email: Some("alice@example.com".to_string()),
            team: None,
            visibility: Visibility::Org,
            version: version.to_string(),
            hash: PackageHash::sha256_of(format!("{org}/{name}@{version}").as_bytes()),
            platform_tags: vec![],
            created_at: Some(created_at.to_string()),
            updated_at: Some(created_at.to_string()),
            status: Some(status),
            current: Some(status == VersionStatus::Approved),
            yanked_at: None,
            yank_reason: None,
            deprecated_at: None,
            deprecation_reason: None,
            install_count: None,
            last_installed_at: None,
            audit_event_id: None,
        }
    }

    #[test]
    fn collects_candidates_across_skills_newest_first() {
        let mock = MockRegistryClient::new();
        mock.seed(
            metadata(
                "acme",
                "alpha",
                "1",
                VersionStatus::Approved,
                "2026-01-01T00:00:00Z",
            ),
            vec![],
        );
        mock.seed(
            metadata(
                "acme",
                "alpha",
                "2",
                VersionStatus::Candidate,
                "2026-01-03T00:00:00Z",
            ),
            vec![],
        );
        mock.seed(
            metadata(
                "acme",
                "beta",
                "1",
                VersionStatus::Candidate,
                "2026-01-02T00:00:00Z",
            ),
            vec![],
        );

        let report = collect_candidates(&mock, "acme", 100).unwrap();
        assert_eq!(report.scanned_skills, 2);
        assert!(!report.truncated);
        assert!(report.skipped.is_empty());
        let refs: Vec<_> = report
            .candidates
            .iter()
            .map(|row| format!("{}/{}@{}", row.org, row.skill, row.version))
            .collect();
        assert_eq!(refs, ["acme/alpha@2", "acme/beta@1"]);
        assert_eq!(
            report.candidates[0].approve_command,
            "agentstack skill version approve acme/alpha@2"
        );
        assert_eq!(
            report.candidates[0].owner.as_deref(),
            Some("alice@example.com")
        );
    }

    #[test]
    fn skips_yanked_and_non_candidate_versions() {
        let mock = MockRegistryClient::new();
        mock.seed(
            metadata(
                "acme",
                "alpha",
                "1",
                VersionStatus::Approved,
                "2026-01-01T00:00:00Z",
            ),
            vec![],
        );
        let mut yanked = metadata(
            "acme",
            "alpha",
            "2",
            VersionStatus::Candidate,
            "2026-01-02T00:00:00Z",
        );
        yanked.yanked_at = Some("2026-01-03T00:00:00Z".to_string());
        yanked.yank_reason = Some("broken".to_string());
        mock.seed(yanked, vec![]);

        let report = collect_candidates(&mock, "acme", 100).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.scanned_skills, 1);
    }

    #[test]
    fn truncates_skill_scan_at_limit() {
        let mock = MockRegistryClient::new();
        for name in ["alpha", "beta", "gamma"] {
            mock.seed(
                metadata(
                    "acme",
                    name,
                    "1",
                    VersionStatus::Candidate,
                    "2026-01-01T00:00:00Z",
                ),
                vec![],
            );
        }

        let report = collect_candidates(&mock, "acme", 2).unwrap();
        assert_eq!(report.scanned_skills, 2);
        assert!(report.truncated);
        assert_eq!(report.candidates.len(), 2);
    }

    #[test]
    fn json_shape_lists_candidates_with_approve_commands() {
        let mock = MockRegistryClient::new();
        mock.seed(
            metadata(
                "acme",
                "alpha",
                "1",
                VersionStatus::Candidate,
                "2026-01-01T00:00:00Z",
            ),
            vec![],
        );
        let report = collect_candidates(&mock, "acme", 100).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&render_json("acme", &report).unwrap()).unwrap();
        assert_eq!(value["scanned_skills"], 1);
        assert_eq!(value["truncated"], false);
        assert!(value.get("empty_message").is_none());
        assert!(value.get("next_command").is_none());
        let row = &value["candidates"][0];
        assert_eq!(row["org"], "acme");
        assert_eq!(row["skill"], "alpha");
        assert_eq!(row["version"], "1");
        assert_eq!(row["created_at"], "2026-01-01T00:00:00Z");
        assert_eq!(row["owner"], "alice@example.com");
        assert_eq!(
            row["approve_command"],
            "agentstack skill version approve acme/alpha@1"
        );
    }

    #[test]
    fn empty_json_exposes_empty_message_and_next_command() {
        let mock = MockRegistryClient::new();
        let report = collect_candidates(&mock, "acme", 100).unwrap();
        assert!(report.candidates.is_empty());
        assert_eq!(report.scanned_skills, 0);
        let value: serde_json::Value =
            serde_json::from_str(&render_json("acme", &report).unwrap()).unwrap();
        assert_eq!(value["candidates"].as_array().unwrap().len(), 0);
        assert_eq!(
            value["empty_message"],
            "no candidate versions awaiting approval in `acme`."
        );
        assert_eq!(value["next_command"], "agentstack skill list --org acme");
    }
}
