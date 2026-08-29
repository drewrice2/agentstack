use std::io::{self, BufRead, Write};

use anyhow::{Context, Result, anyhow, bail};

use crate::output::Ctx;
use crate::registry::{
    OrgMembership, RegistryClient, RemoteSkill, SearchFilters, StackListFilters, StackSummary,
};
use crate::skill::check_slug;
use crate::skill_ref::{SkillRef, check_version};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillRefInput {
    Qualified(SkillRef),
    Relative {
        name: String,
        version: Option<String>,
    },
}

impl SkillRefInput {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.contains('/') {
            return Ok(Self::Qualified(raw.parse()?));
        }

        let (name, version) = parse_name_version(raw)?;
        Ok(Self::Relative { name, version })
    }

    pub fn requires_org_resolution(&self) -> bool {
        matches!(self, Self::Relative { .. })
    }

    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Qualified(skill_ref) => skill_ref.version.as_deref(),
            Self::Relative { version, .. } => version.as_deref(),
        }
    }
}

pub fn validate_skill_ref_input(raw: &str) -> Result<SkillRefInput> {
    SkillRefInput::parse(raw)
}

pub fn validate_skill_ref_input_with_team(raw: &str, team: Option<&str>) -> Result<SkillRefInput> {
    let input = SkillRefInput::parse(raw)?;
    if let Some(team) = team {
        check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
    }
    Ok(input)
}

pub fn parse_relative_skill_ref(raw: &str) -> Result<(String, Option<String>)> {
    parse_name_version(raw)
}

pub fn resolve_skill_ref(ctx: &Ctx, client: &dyn RegistryClient, raw: &str) -> Result<SkillRef> {
    resolve_skill_ref_with_team(ctx, client, raw, None)
}

pub fn resolve_skill_ref_with_team(
    ctx: &Ctx,
    client: &dyn RegistryClient,
    raw: &str,
    team: Option<&str>,
) -> Result<SkillRef> {
    match validate_skill_ref_input_with_team(raw, team)? {
        SkillRefInput::Qualified(skill_ref) => {
            if let Some(team) = team {
                ensure_team_skill_visible(client, &skill_ref.org, team, &skill_ref.name)?;
            }
            Ok(skill_ref)
        }
        SkillRefInput::Relative { name, version } => {
            let org = resolve_token_org(ctx, client, raw)?;
            if let Some(team) = team {
                let resolved = resolve_team_skill(client, &org, team, &name)?;
                return resolved.with_optional_version(version);
            }
            SkillRef::new(org, name)?.with_optional_version(version)
        }
    }
}

pub fn resolve_stack_ref(
    ctx: &Ctx,
    client: &dyn RegistryClient,
    raw: &str,
    legacy_org: Option<&str>,
) -> Result<(String, String)> {
    resolve_stack_ref_with_team(ctx, client, raw, legacy_org, None)
}

pub fn resolve_stack_ref_with_team(
    ctx: &Ctx,
    client: &dyn RegistryClient,
    raw: &str,
    legacy_org: Option<&str>,
    team: Option<&str>,
) -> Result<(String, String)> {
    if let Some(org) = legacy_org {
        check_slug(org).map_err(|reason| anyhow!("invalid --org `{org}`: {reason}"))?;
        check_slug(raw).map_err(|reason| anyhow!("invalid stack `{raw}`: {reason}"))?;
        if let Some(team) = team {
            check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
            ensure_team_stack_visible(client, org, team, raw)?;
        }
        return Ok((org.to_string(), raw.to_string()));
    }

    let slash_count = raw.matches('/').count();
    if slash_count == 1 {
        let (org, stack) = raw
            .split_once('/')
            .ok_or_else(|| anyhow!("stack ref must be in the form `org/stack`"))?;
        check_slug(org).map_err(|reason| anyhow!("invalid org `{org}`: {reason}"))?;
        check_slug(stack).map_err(|reason| anyhow!("invalid stack `{stack}`: {reason}"))?;
        if let Some(team) = team {
            check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
            ensure_team_stack_visible(client, org, team, stack)?;
        }
        return Ok((org.to_string(), stack.to_string()));
    }
    if slash_count > 1 {
        bail!("stack ref must be in the form `org/stack` or `stack`");
    }

    check_slug(raw).map_err(|reason| anyhow!("invalid stack `{raw}`: {reason}"))?;
    let org = resolve_token_org(ctx, client, raw)?;
    if let Some(team) = team {
        check_slug(team).map_err(|reason| anyhow!("invalid --team `{team}`: {reason}"))?;
        return resolve_team_stack(client, &org, team, raw);
    }
    Ok((org, raw.to_string()))
}

pub fn resolve_token_org(ctx: &Ctx, client: &dyn RegistryClient, raw_ref: &str) -> Result<String> {
    let identity = client.whoami().map_err(|err| {
        anyhow!("could not infer org for `{raw_ref}` from the active token: {err}")
    })?;

    let mut orgs = identity.orgs;
    orgs.sort_by(|a, b| a.slug.cmp(&b.slug));
    orgs.dedup_by(|a, b| a.slug == b.slug);

    match orgs.as_slice() {
        [org] => Ok(org.slug.clone()),
        [] => match identity.org {
            Some(org) => Ok(org),
            None => bail!(
                "could not infer org for `{raw_ref}` because the active token has no org memberships; {}",
                org_hint(raw_ref)
            ),
        },
        many if ctx.can_prompt() => prompt_for_org(raw_ref, many),
        many => bail!(
            "could not infer org for `{raw_ref}` because the active token can access multiple orgs: {}. {} or rerun interactively to choose.",
            many.iter()
                .map(|org| org.slug.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            org_hint(raw_ref)
        ),
    }
}

fn org_hint(raw_ref: &str) -> String {
    if raw_ref.contains(char::is_whitespace) {
        "pass `--org <org>`".to_string()
    } else {
        format!("use `org/{raw_ref}`")
    }
}

fn parse_name_version(raw: &str) -> Result<(String, Option<String>)> {
    let (name, version) = match raw.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (raw, None),
    };
    SkillRef::new("org", name)?;
    let version = match version {
        Some(version) => Some(check_version(version)?.to_string()),
        None => None,
    };
    Ok((name.to_string(), version))
}

fn resolve_team_skill(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    name: &str,
) -> Result<SkillRef> {
    let matches = team_skill_matches(client, org, team, name)?;

    match matches.as_slice() {
        [skill] => SkillRef::new(skill.org.clone(), skill.name.clone()).map_err(Into::into),
        [] => bail!("no skill `{name}` is visible for team `{team}` in org `{org}`"),
        many => bail!(
            "skill `{name}` matched multiple team-visible resources: {}",
            many.iter()
                .map(|skill| skill.skill_ref())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn ensure_team_skill_visible(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    name: &str,
) -> Result<()> {
    let matches = team_skill_matches(client, org, team, name)?;

    match matches.as_slice() {
        [_] => Ok(()),
        [] => bail!("no skill `{name}` is visible for team `{team}` in org `{org}`"),
        many => bail!(
            "skill `{name}` matched multiple team-visible resources: {}",
            many.iter()
                .map(|skill| skill.skill_ref())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn team_skill_matches(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    name: &str,
) -> Result<Vec<RemoteSkill>> {
    Ok(client
        .list_remote_with_filters(&SearchFilters {
            org: Some(org.to_string()),
            team: Some(team.to_string()),
            ..SearchFilters::default()
        })?
        .into_iter()
        .filter(|skill| skill.name == name && skill.team.as_deref() == Some(team))
        .collect())
}

fn resolve_team_stack(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    stack: &str,
) -> Result<(String, String)> {
    let matches = team_stack_matches(client, org, team, stack)?;

    match matches.as_slice() {
        [row] => Ok((row.org.clone(), row.slug.clone())),
        [] => bail!("no stack `{stack}` is visible for team `{team}` in org `{org}`"),
        many => bail!(
            "stack `{stack}` matched multiple team-visible resources: {}",
            many.iter()
                .map(|row| format!("{}/{}", row.org, row.slug))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn ensure_team_stack_visible(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    stack: &str,
) -> Result<()> {
    let matches = team_stack_matches(client, org, team, stack)?;

    match matches.as_slice() {
        [_] => Ok(()),
        [] => bail!("no stack `{stack}` is visible for team `{team}` in org `{org}`"),
        many => bail!(
            "stack `{stack}` matched multiple team-visible resources: {}",
            many.iter()
                .map(|row| format!("{}/{}", row.org, row.slug))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn team_stack_matches(
    client: &dyn RegistryClient,
    org: &str,
    team: &str,
    stack: &str,
) -> Result<Vec<StackSummary>> {
    Ok(client
        .list_stacks_with_filters(
            org,
            &StackListFilters {
                owner: None,
                team: Some(team.to_string()),
                limit: None,
            },
        )?
        .into_iter()
        .filter(|row| row.slug == stack && row.team.as_deref() == Some(team))
        .collect())
}

fn prompt_for_org(raw_ref: &str, orgs: &[OrgMembership]) -> Result<String> {
    eprintln!("`{raw_ref}` is available as an org-relative ref. Choose an org:");
    for (index, org) in orgs.iter().enumerate() {
        eprintln!("  {}) {} ({})", index + 1, org.slug, org.role);
    }
    eprint!("Org [1-{}]: ", orgs.len());
    io::stderr().flush().context("failed to flush org prompt")?;

    let mut line = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut line)
        .context("failed to read org selection")?;
    if read == 0 {
        bail!("no org selected; use `org/{raw_ref}`");
    }
    let trimmed = line.trim();
    let selected = trimmed
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid org selection `{trimmed}`"))?;
    let org = orgs
        .get(selected.saturating_sub(1))
        .ok_or_else(|| anyhow!("org selection `{selected}` is out of range"))?;
    Ok(org.slug.clone())
}

trait WithOptionalVersion {
    fn with_optional_version(self, version: Option<String>) -> Result<SkillRef>;
}

impl WithOptionalVersion for SkillRef {
    fn with_optional_version(self, version: Option<String>) -> Result<SkillRef> {
        match version {
            Some(version) => Ok(self.with_version(version)?),
            None => Ok(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::PackageHash;
    use crate::registry::{
        MockRegistryClient, PushRequest, RegistryClient, SkillMetadata, VersionStatus, Visibility,
        WhoamiResponse,
    };

    fn ctx() -> Ctx {
        Ctx {
            no_input: true,
            ..Ctx::default()
        }
    }

    fn user_with_orgs(orgs: &[&str]) -> WhoamiResponse {
        WhoamiResponse {
            user: "alice@example.com".to_string(),
            org: None,
            email: "alice@example.com".to_string(),
            name: None,
            server_admin: false,
            orgs: orgs
                .iter()
                .map(|org| OrgMembership {
                    slug: (*org).to_string(),
                    name: (*org).to_string(),
                    role: "reader".to_string(),
                })
                .collect(),
        }
    }

    fn metadata(org: &str, name: &str, team: Option<&str>) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: format!("Use when working on {name}"),
            org: org.to_string(),
            owner_email: None,
            team: team.map(str::to_string),
            visibility: if team.is_some() {
                Visibility::Team
            } else {
                Visibility::Org
            },
            version: "1".to_string(),
            hash: PackageHash::sha256_of(format!("{org}/{name}@1").as_bytes()),
            platform_tags: vec![],
            created_at: None,
            updated_at: None,
            install_count: None,
            last_installed_at: None,
            status: Some(VersionStatus::Candidate),
            current: None,
            yanked_at: None,
            yank_reason: None,
            deprecated_at: None,
            deprecation_reason: None,
            audit_event_id: None,
        }
    }

    #[test]
    fn parses_relative_skill_refs() {
        assert_eq!(
            SkillRefInput::parse("code-review").unwrap(),
            SkillRefInput::Relative {
                name: "code-review".to_string(),
                version: None,
            }
        );
        assert_eq!(
            SkillRefInput::parse("code-review@3").unwrap(),
            SkillRefInput::Relative {
                name: "code-review".to_string(),
                version: Some("3".to_string()),
            }
        );
    }

    #[test]
    fn keeps_qualified_skill_refs_canonical() {
        let parsed = SkillRefInput::parse("acme/code-review@3").unwrap();
        assert_eq!(
            parsed,
            SkillRefInput::Qualified(SkillRef::parse("acme/code-review@3").unwrap())
        );
    }

    #[test]
    fn resolves_relative_skill_ref_from_single_org_token() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));

        let resolved = resolve_skill_ref(&ctx(), &mock, "code-review@3").unwrap();

        assert_eq!(resolved.to_string(), "acme/code-review@3");
    }

    #[test]
    fn rejects_relative_skill_ref_for_multi_org_token_without_prompt() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme", "other"]));

        let err = resolve_skill_ref(&ctx(), &mock, "code-review")
            .unwrap_err()
            .to_string();

        assert!(err.contains("multiple orgs"), "err: {err}");
        assert!(err.contains("acme, other"), "err: {err}");
        assert!(err.contains("org/code-review"), "err: {err}");
    }

    #[test]
    fn rejects_relative_skill_ref_for_token_without_orgs() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&[]));

        let err = resolve_skill_ref(&ctx(), &mock, "code-review")
            .unwrap_err()
            .to_string();

        assert!(err.contains("no org memberships"), "err: {err}");
    }

    #[test]
    fn resolves_relative_stack_ref_from_single_org_token() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));

        let resolved = resolve_stack_ref(&ctx(), &mock, "engineering-default", None).unwrap();

        assert_eq!(
            resolved,
            ("acme".to_string(), "engineering-default".to_string())
        );
    }

    #[test]
    fn resolves_team_skill_ref_through_visible_team_catalog() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.push(PushRequest {
            metadata: metadata("acme", "code-review", Some("platform")),
            archive: b"skill",
        })
        .unwrap();

        let resolved =
            resolve_skill_ref_with_team(&ctx(), &mock, "code-review@1", Some("platform")).unwrap();

        assert_eq!(resolved.to_string(), "acme/code-review@1");
    }

    #[test]
    fn resolves_qualified_team_skill_ref_when_visible() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.push(PushRequest {
            metadata: metadata("acme", "code-review", Some("platform")),
            archive: b"skill",
        })
        .unwrap();

        let resolved =
            resolve_skill_ref_with_team(&ctx(), &mock, "acme/code-review@1", Some("platform"))
                .unwrap();

        assert_eq!(resolved.to_string(), "acme/code-review@1");
    }

    #[test]
    fn rejects_qualified_team_skill_ref_when_not_visible() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.push(PushRequest {
            metadata: metadata("acme", "code-review", Some("platform")),
            archive: b"skill",
        })
        .unwrap();

        let err = resolve_skill_ref_with_team(&ctx(), &mock, "acme/code-review", Some("design"))
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("no skill `code-review` is visible for team `design` in org `acme`"),
            "err: {err}"
        );
    }

    #[test]
    fn resolves_team_stack_ref_through_visible_team_catalog() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.create_stack(
            "acme",
            "engineering-default",
            "Engineering Default",
            "",
            Visibility::Team,
            Some("platform"),
        )
        .unwrap();

        let resolved = resolve_stack_ref_with_team(
            &ctx(),
            &mock,
            "engineering-default",
            None,
            Some("platform"),
        )
        .unwrap();

        assert_eq!(
            resolved,
            ("acme".to_string(), "engineering-default".to_string())
        );
    }

    #[test]
    fn resolves_qualified_team_stack_ref_when_visible() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.create_stack(
            "acme",
            "engineering-default",
            "Engineering Default",
            "",
            Visibility::Team,
            Some("platform"),
        )
        .unwrap();

        let resolved = resolve_stack_ref_with_team(
            &ctx(),
            &mock,
            "acme/engineering-default",
            None,
            Some("platform"),
        )
        .unwrap();

        assert_eq!(
            resolved,
            ("acme".to_string(), "engineering-default".to_string())
        );
    }

    #[test]
    fn rejects_qualified_team_stack_ref_when_not_visible() {
        let mock = MockRegistryClient::new();
        mock.set_user(user_with_orgs(&["acme"]));
        mock.create_stack(
            "acme",
            "engineering-default",
            "Engineering Default",
            "",
            Visibility::Team,
            Some("platform"),
        )
        .unwrap();

        let err = resolve_stack_ref_with_team(
            &ctx(),
            &mock,
            "acme/engineering-default",
            None,
            Some("design"),
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains(
                "no stack `engineering-default` is visible for team `design` in org `acme`"
            ),
            "err: {err}"
        );
    }
}
