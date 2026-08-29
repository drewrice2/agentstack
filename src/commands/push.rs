//! `agentstack skill push` — validate, lint, pack, and publish a skill to the
//! active registry.
//!
//! The handler is split into two layers so the workflow can be unit-tested
//! against a [`MockRegistryClient`] without going through the binary:
//!
//! - [`run`] resolves the active registry + token and constructs a
//!   real HTTP registry client for live uploads.
//! - [`run_with_client`] takes any [`RegistryClient`] and runs the full
//!   validate -> lint -> pack -> skill push/dry-run pipeline.
//!
//! [`MockRegistryClient`]: crate::registry::MockRegistryClient

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::{configured_client, registry_context};
use super::refs;
use crate::output::Ctx;
use crate::package::build_skill_package;
use crate::registry::{PushRequest, RegistryClient, SkillMetadata, Visibility};
use crate::skill::{
    DEFAULT_SOFT_CHAR_LIMIT, DiscoveredSkill, LintConfig, LintWarning, ValidationOutcome,
    discover_skills, lint_skill, validate_skill,
};

pub struct Args {
    pub path: Option<PathBuf>,
    pub org: Option<String>,
    pub visibility: String,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub dry_run: bool,
    pub all: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub yes: bool,
}

/// Resolved CLI args — kept distinct from [`Args`] so unit tests can build
/// the typed shape directly without going through clap.
pub struct PushOptions<'a> {
    pub source: &'a Path,
    pub org: &'a str,
    pub visibility: Visibility,
    pub team: Option<&'a str>,
    pub platforms: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct PushOutcome {
    pub metadata: SkillMetadata,
    pub skill_ref: String,
    pub version: String,
    pub sha256: String,
    pub visibility: Visibility,
    pub size_bytes: u64,
    pub url: Option<String>,
    pub audit_event_id: Option<String>,
    pub lint_warnings: Vec<LintWarning>,
    pub skipped_symlinks: Vec<String>,
    pub dry_run: bool,
    pub authorization_checked: bool,
}

pub struct PushAllOptions<'a> {
    pub root: &'a Path,
    pub org: &'a str,
    pub visibility: Visibility,
    pub team: Option<String>,
    pub platforms: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct BatchOutcome {
    pub org: String,
    pub path: PathBuf,
    pub dry_run: bool,
    pub pushed: Vec<BatchPushed>,
    pub skipped: Vec<BatchSkipped>,
    pub failed: Vec<BatchFailed>,
}

#[derive(Debug, Clone)]
pub struct BatchPushed {
    pub name: String,
    pub skill_ref: String,
    pub version: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub audit_event_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BatchSkipped {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct BatchFailed {
    pub name: String,
    pub reason: String,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let source = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    let visibility: Visibility = args.visibility.parse()?;
    let org_was_inferred = args.org.is_none();
    let org = resolve_org(ctx, args.org.as_deref())?;
    crate::skill::check_slug(&org)
        .map_err(|reason| anyhow::anyhow!("invalid --org `{org}`: {reason}"))?;
    validate_team_arg(visibility, args.team.as_deref())?;

    if args.all {
        return run_all(ctx, source, args, &org, visibility);
    }

    let _ = require_valid_skill(&source, Some(ctx))?;

    if args.dry_run {
        let mut outcome = run_with_client(
            None,
            None,
            PushOptions {
                source: &source,
                org: &org,
                visibility,
                team: args.team.as_deref(),
                platforms: args.platforms,
                dry_run: true,
            },
        )?;
        outcome.authorization_checked = org_was_inferred;
        return render_single(ctx, &outcome, None);
    }

    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));

    if !args.yes {
        let confirmed = ctx.prompt_confirm(
            format!("Push {} to {org}?", source.display()),
            "skill push cannot prompt in this context; rerun with `--yes` or run interactively",
        )?;
        if !confirmed {
            ctx.say("no changes made");
            return Ok(());
        }
    }

    let outcome = run_with_client(
        Some(&configured.client),
        Some(&configured.url),
        PushOptions {
            source: &source,
            org: &org,
            visibility,
            team: args.team.as_deref(),
            platforms: args.platforms,
            dry_run: false,
        },
    )?;
    render_single(ctx, &outcome, Some(&configured.url))
}

pub(super) fn validate_team_arg(visibility: Visibility, team: Option<&str>) -> Result<()> {
    match (visibility, team) {
        (Visibility::Team, Some(team)) => {
            crate::skill::check_slug(team)
                .map_err(|reason| anyhow::anyhow!("invalid --team `{team}`: {reason}"))?;
            Ok(())
        }
        (Visibility::Team, None) => bail!("--team is required when --scope team is used"),
        (Visibility::Private | Visibility::Org, Some(_)) => {
            bail!("--team can only be used with --scope team")
        }
        (Visibility::Private | Visibility::Org, None) => Ok(()),
    }
}

fn run_all(ctx: &Ctx, root: PathBuf, args: Args, org: &str, visibility: Visibility) -> Result<()> {
    let plan = build_batch_plan(&root, &args.include, &args.exclude)?;
    let opts = PushAllOptions {
        root: &plan.root,
        org,
        visibility,
        team: args.team,
        platforms: args.platforms,
        include: args.include,
        exclude: args.exclude,
        dry_run: args.dry_run,
    };

    if ctx.json {
        if plan.selected_count() == 0 {
            let outcome = execute_batch_with_client(None, None, &plan, opts, None);
            render_batch_json(ctx, &outcome)?;
            return Ok(());
        }
    } else {
        render_batch_table(ctx, &plan);
        if plan.selected_count() == 0 {
            ctx.say(format!(
                "no skills selected to push under {}",
                plan.root.display()
            ));
            ctx.say("next: agentstack skill scan");
            return Ok(());
        }
    }

    let push_count = plan.selected_valid_count();
    if push_count > 0 && !args.dry_run && !args.yes {
        let confirmed = ctx.prompt_confirm(
            format!("Push {push_count} skill(s) to {org}?"),
            "push --all cannot prompt in this context; rerun with `--yes` or run interactively",
        )?;
        if !confirmed {
            ctx.say("no changes made");
            return Ok(());
        }
    }

    if !args.dry_run && push_count > 0 {
        let configured = configured_client()?;
        ctx.verbose(format!("registry: {}", configured.url));
        return execute_and_render_batch(
            ctx,
            Some(&configured.client),
            Some(&configured.url),
            &plan,
            opts,
        );
    }

    execute_and_render_batch(ctx, None, None, &plan, opts)
}

fn resolve_org(ctx: &Ctx, explicit_org: Option<&str>) -> Result<String> {
    if let Some(org) = explicit_org {
        return Ok(org.to_string());
    }
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    refs::resolve_token_org(ctx, &configured.client, "skill push")
}

fn execute_and_render_batch(
    ctx: &Ctx,
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    plan: &BatchPlan,
    opts: PushAllOptions<'_>,
) -> Result<()> {
    let outcome = execute_batch_with_client(client, registry_url, plan, opts, Some(ctx));
    if ctx.json {
        render_batch_json(ctx, &outcome)?;
    } else {
        render_batch_summary(ctx, &outcome);
    }
    if !outcome.failed.is_empty() {
        bail!(
            "push --all failed for {} skill{}",
            outcome.failed.len(),
            if outcome.failed.len() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// Batch workflow used by integration tests. It performs discovery, filtering,
/// and sequential push attempts, but leaves CLI prompting and rendering to
/// [`run`].
pub fn run_all_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    opts: PushAllOptions<'_>,
) -> Result<BatchOutcome> {
    crate::skill::check_slug(opts.org)
        .map_err(|reason| anyhow::anyhow!("invalid --org `{}`: {reason}", opts.org))?;
    validate_team_arg(opts.visibility, opts.team.as_deref())?;
    let plan = build_batch_plan(opts.root, &opts.include, &opts.exclude)?;
    Ok(execute_batch_with_client(
        client,
        registry_url,
        &plan,
        opts,
        None,
    ))
}

/// Workflow used by both the CLI and the integration tests.
pub fn run_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    opts: PushOptions<'_>,
) -> Result<PushOutcome> {
    crate::skill::check_slug(opts.org)
        .map_err(|reason| anyhow::anyhow!("invalid --org `{}`: {reason}", opts.org))?;
    validate_team_arg(opts.visibility, opts.team)?;

    let outcome = require_valid_skill(opts.source, None)?;

    let lint_warnings = if let (Some(parsed), Some(content)) =
        (outcome.parsed.as_ref(), outcome.content.as_deref())
    {
        lint_skill(
            opts.source,
            parsed,
            content,
            &LintConfig {
                soft_char_limit: DEFAULT_SOFT_CHAR_LIMIT,
            },
        )
    } else {
        Vec::new()
    };

    push_validated_skill_with_client(client, registry_url, opts, lint_warnings)
}

fn push_validated_skill_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    opts: PushOptions<'_>,
    lint_warnings: Vec<LintWarning>,
) -> Result<PushOutcome> {
    let built = build_skill_package(opts.source)
        .with_context(|| format!("failed to pack `{}`", opts.source.display()))?;

    let version = built.manifest.version.clone();
    crate::skill_ref::check_version(&version)?;

    let metadata = SkillMetadata {
        name: built.manifest.name.clone(),
        description: built.manifest.description.clone(),
        org: opts.org.to_string(),
        owner_email: None,
        team: opts.team.map(str::to_string),
        visibility: opts.visibility,
        version,
        hash: built.hash.clone(),
        platform_tags: opts.platforms,
        created_at: None,
        updated_at: None,
        install_count: None,
        last_installed_at: None,
        status: None,
        current: None,
        yanked_at: None,
        yank_reason: None,
        deprecated_at: None,
        deprecation_reason: None,
        audit_event_id: None,
    };
    let size_bytes = built.bytes.len() as u64;
    let skipped_symlinks = built.skipped_symlinks;

    if opts.dry_run {
        let skill_ref = metadata.skill_ref();
        let version = metadata.version.clone();
        let sha256 = metadata.hash.hex.clone();
        let visibility = metadata.visibility;
        return Ok(PushOutcome {
            metadata,
            skill_ref,
            version,
            sha256,
            visibility,
            size_bytes,
            url: None,
            audit_event_id: None,
            lint_warnings,
            skipped_symlinks,
            dry_run: true,
            authorization_checked: false,
        });
    }

    let client = client.expect("registry client required for non-dry-run push");
    let response = client
        .push(PushRequest {
            metadata: metadata.clone(),
            archive: &built.bytes,
        })
        .with_context(|| registry_context(registry_url, "push to", "push"))?;

    let mut metadata = response.metadata;
    if metadata.audit_event_id.is_none() {
        metadata.audit_event_id = response.audit_event_id.clone();
    }

    Ok(PushOutcome {
        metadata,
        skill_ref: response.skill_ref,
        version: response.version,
        sha256: response.sha256,
        visibility: response.visibility,
        size_bytes,
        url: response.url,
        audit_event_id: response.audit_event_id,
        lint_warnings,
        skipped_symlinks,
        dry_run: false,
        authorization_checked: true,
    })
}

fn require_valid_skill(source: &Path, ctx: Option<&Ctx>) -> Result<ValidationOutcome> {
    let outcome = validate_skill(source);
    if outcome.is_ok() {
        return Ok(outcome);
    }
    if let Some(ctx) = ctx
        && !ctx.json
    {
        for err in &outcome.errors {
            ctx.warn(err.to_string());
        }
    }
    // Each error is already printed above via `warn`; the summary just counts
    // them (matching `validate`/`pack`) instead of repeating the first message.
    anyhow::bail!(
        "`{}` is not a valid skill ({} error{}); fix and rerun",
        source.display(),
        outcome.errors.len(),
        if outcome.errors.len() == 1 { "" } else { "s" },
    );
}

#[derive(Debug)]
struct BatchPlan {
    root: PathBuf,
    rows: Vec<PlannedSkill>,
}

impl BatchPlan {
    fn selected_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| {
                matches!(
                    row.status,
                    PlannedStatus::Selected | PlannedStatus::Invalid(_)
                )
            })
            .count()
    }

    fn selected_valid_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row.status, PlannedStatus::Selected))
            .count()
    }
}

#[derive(Debug)]
struct PlannedSkill {
    name: String,
    path: PathBuf,
    status: PlannedStatus,
    lint_warnings: usize,
}

#[derive(Debug)]
enum PlannedStatus {
    Selected,
    Invalid(String),
    Excluded(&'static str),
}

fn build_batch_plan(root: &Path, include: &[String], exclude: &[String]) -> Result<BatchPlan> {
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to read `{}`", root.display()))?;
    let discovered = discover_skills(&root)?;
    let rows = discovered
        .into_iter()
        .map(|skill| plan_discovered_skill(skill, include, exclude))
        .collect();
    Ok(BatchPlan { root, rows })
}

fn plan_discovered_skill(
    skill: DiscoveredSkill,
    include: &[String],
    exclude: &[String],
) -> PlannedSkill {
    let included = include.is_empty()
        || include
            .iter()
            .any(|pattern| glob_match(pattern, &skill.name));
    let excluded = exclude
        .iter()
        .any(|pattern| glob_match(pattern, &skill.name));

    let status = if !included {
        PlannedStatus::Excluded("excluded by --include")
    } else if excluded {
        PlannedStatus::Excluded("excluded by --exclude")
    } else if !skill.validation.is_ok() {
        PlannedStatus::Invalid(first_validation_reason(&skill.validation))
    } else {
        PlannedStatus::Selected
    };
    let lint_warnings = if matches!(status, PlannedStatus::Selected) {
        lint_warning_count(&skill)
    } else {
        0
    };

    PlannedSkill {
        name: skill.name,
        path: skill.path,
        status,
        lint_warnings,
    }
}

fn lint_warning_count(skill: &DiscoveredSkill) -> usize {
    let Some(parsed) = skill.validation.parsed.as_ref() else {
        return 0;
    };
    let Some(content) = skill.validation.content.as_deref() else {
        return 0;
    };
    lint_skill(
        &skill.path,
        parsed,
        content,
        &LintConfig {
            soft_char_limit: DEFAULT_SOFT_CHAR_LIMIT,
        },
    )
    .len()
}

fn first_validation_reason(validation: &ValidationOutcome) -> String {
    validation
        .errors
        .first()
        .map(|err| err.code.as_str().to_string())
        .unwrap_or_else(|| "validation_error".to_string())
}

fn execute_batch_with_client(
    client: Option<&dyn RegistryClient>,
    registry_url: Option<&str>,
    plan: &BatchPlan,
    opts: PushAllOptions<'_>,
    ctx: Option<&Ctx>,
) -> BatchOutcome {
    let mut outcome = empty_batch_outcome(plan, opts.org);
    outcome.dry_run = opts.dry_run;
    let total = plan.selected_valid_count();
    let mut index = 0usize;

    for row in &plan.rows {
        match &row.status {
            PlannedStatus::Excluded(_) => {
                outcome.skipped.push(BatchSkipped {
                    name: row.name.clone(),
                    reason: "excluded".to_string(),
                });
            }
            PlannedStatus::Invalid(reason) => {
                outcome.failed.push(BatchFailed {
                    name: row.name.clone(),
                    reason: reason.clone(),
                });
            }
            PlannedStatus::Selected => {
                index += 1;
                let result = push_validated_skill_with_client(
                    client,
                    registry_url,
                    PushOptions {
                        source: &row.path,
                        org: opts.org,
                        visibility: opts.visibility,
                        team: opts.team.as_deref(),
                        platforms: opts.platforms.clone(),
                        dry_run: opts.dry_run,
                    },
                    Vec::new(),
                );
                match result {
                    Ok(pushed) => {
                        render_batch_progress(ctx, index, total, row, Ok(&pushed), opts.dry_run);
                        outcome.pushed.push(BatchPushed {
                            name: row.name.clone(),
                            skill_ref: pushed.skill_ref,
                            version: pushed.version,
                            sha256: pushed.sha256,
                            size_bytes: pushed.size_bytes,
                            audit_event_id: pushed.audit_event_id,
                        });
                    }
                    Err(err) => {
                        let reason = format!("{err:#}");
                        render_batch_progress(
                            ctx,
                            index,
                            total,
                            row,
                            Err(reason.as_str()),
                            opts.dry_run,
                        );
                        outcome.failed.push(BatchFailed {
                            name: row.name.clone(),
                            reason,
                        });
                    }
                }
            }
        }
    }

    outcome
}

fn empty_batch_outcome(plan: &BatchPlan, org: &str) -> BatchOutcome {
    BatchOutcome {
        org: org.to_string(),
        path: plan.root.clone(),
        dry_run: false,
        pushed: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
    }
}

fn render_single(ctx: &Ctx, outcome: &PushOutcome, registry_url: Option<&str>) -> Result<()> {
    render_lint_warnings(ctx, &outcome.lint_warnings);
    if !ctx.json {
        super::pack::warn_skipped_symlinks(ctx, &outcome.skipped_symlinks);
    }

    if outcome.dry_run {
        if ctx.json {
            ctx.say_always(render_dry_run_json(outcome)?);
        } else {
            render_dry_run_human(ctx, outcome);
        }
        return Ok(());
    }

    if ctx.json {
        ctx.say_always(render_json(outcome)?);
    } else {
        render_human(ctx, outcome, registry_url);
    }
    Ok(())
}

fn render_lint_warnings(ctx: &Ctx, warnings: &[LintWarning]) {
    if ctx.json || warnings.is_empty() {
        return;
    }
    ctx.warn("lint warnings:");
    for warning in warnings {
        ctx.warn(format!("  - [{}] {}", warning.code, warning.message));
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    metadata: &'a SkillMetadata,
    skill_ref: &'a str,
    version: &'a str,
    sha256: &'a str,
    visibility: Visibility,
    lint_warnings: &'a [LintWarning],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    skipped_symlinks: &'a [String],
    url: Option<&'a str>,
    audit_event_id: Option<&'a str>,
    next_commands: Vec<String>,
}

fn render_json(outcome: &PushOutcome) -> Result<String> {
    let out = JsonOutput {
        metadata: &outcome.metadata,
        skill_ref: &outcome.skill_ref,
        version: &outcome.version,
        sha256: &outcome.sha256,
        visibility: outcome.visibility,
        lint_warnings: &outcome.lint_warnings,
        skipped_symlinks: &outcome.skipped_symlinks,
        url: outcome.url.as_deref(),
        audit_event_id: outcome.audit_event_id.as_deref(),
        next_commands: push_next_commands(outcome),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn push_next_commands(outcome: &PushOutcome) -> Vec<String> {
    let unversioned = format!("{}/{}", outcome.metadata.org, outcome.metadata.name);
    let mut commands = Vec::new();
    if outcome.metadata.current == Some(true) {
        commands.push(format!("agentstack skill status {unversioned}"));
    } else {
        commands.push(format!(
            "agentstack skill version approve {}@{}",
            unversioned, outcome.metadata.version
        ));
    }
    commands.push(format!("agentstack skill version list {unversioned}"));
    commands
}

#[derive(Serialize)]
struct DryRunJsonOutput<'a> {
    would_upload: bool,
    authorization_checked: bool,
    metadata: &'a SkillMetadata,
    skill_ref: &'a str,
    version: &'a str,
    sha256: &'a str,
    visibility: Visibility,
    lint_warnings: &'a [LintWarning],
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    skipped_symlinks: &'a [String],
    size_bytes: u64,
}

fn render_dry_run_json(outcome: &PushOutcome) -> Result<String> {
    let out = DryRunJsonOutput {
        would_upload: true,
        authorization_checked: outcome.authorization_checked,
        metadata: &outcome.metadata,
        skill_ref: &outcome.skill_ref,
        version: &outcome.version,
        sha256: &outcome.sha256,
        visibility: outcome.visibility,
        lint_warnings: &outcome.lint_warnings,
        skipped_symlinks: &outcome.skipped_symlinks,
        size_bytes: outcome.size_bytes,
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_dry_run_human(ctx: &Ctx, outcome: &PushOutcome) {
    ctx.say(format!("local push plan for {}", outcome.skill_ref));
    match outcome.metadata.visibility {
        Visibility::Private => ctx.say(
            "  visibility: private — only you and admins would see this; use --scope org to share with the org.",
        ),
        Visibility::Org => ctx.say(format!("  visibility: {}", outcome.metadata.visibility)),
        Visibility::Team => ctx.say(format!("  visibility: {}", outcome.metadata.visibility)),
    }
    if let Some(team) = &outcome.metadata.team {
        ctx.say(format!("  team:       {team}"));
    }
    ctx.say(format!("  version:    {}", outcome.version));
    ctx.say(format!("  hash:       {}", outcome.sha256));
    ctx.say(format!(
        "  size:       {}",
        crate::cache::human_bytes(outcome.size_bytes)
    ));
    if !outcome.metadata.platform_tags.is_empty() {
        ctx.say(format!(
            "  platforms:  {}",
            outcome.metadata.platform_tags.join(", ")
        ));
    }
    if outcome.authorization_checked {
        ctx.say("authorization: org inferred from the active token; upload not attempted");
    } else {
        ctx.say("authorization: not checked (dry run does not contact the registry)");
    }
    ctx.say("dry run; not uploaded");
}

fn render_human(ctx: &Ctx, outcome: &PushOutcome, registry_url: Option<&str>) {
    ctx.say(format!("pushed {}", outcome.metadata.skill_ref()));
    let unversioned = format!("{}/{}", outcome.metadata.org, outcome.metadata.name);
    let visibility_note = match outcome.metadata.visibility {
        Visibility::Private => Some(
            "private — only you and admins can see this; use --scope org to share with the org.",
        ),
        Visibility::Org => None,
        Visibility::Team => None,
    };
    match visibility_note {
        Some(note) => ctx.say(format!("  visibility: {note}")),
        None => ctx.say(format!("  visibility: {}", outcome.metadata.visibility)),
    }
    if let Some(team) = &outcome.metadata.team {
        ctx.say(format!("  team:       {team}"));
    }
    ctx.say(format!("  version:    {}", outcome.metadata.version));
    if let Some(status) = outcome.metadata.status {
        ctx.say(format!("  status:     {status}"));
    }
    if let Some(current) = outcome.metadata.current {
        ctx.say(format!(
            "  current:    {}",
            if current { "yes" } else { "no" }
        ));
    }
    ctx.say(format!("  hash:       {}", outcome.metadata.hash.hex));
    ctx.say(format!(
        "  size:       {}",
        crate::cache::human_bytes(outcome.size_bytes)
    ));
    if !outcome.metadata.platform_tags.is_empty() {
        ctx.say(format!(
            "  platforms:  {}",
            outcome.metadata.platform_tags.join(", ")
        ));
    }
    if let Some(url) = &outcome.url {
        ctx.say(format!("  url:        {url}"));
    } else if let Some(url) = registry_url {
        ctx.say(format!("  registry:   {url}"));
    }
    if let Some(t) = &outcome.metadata.created_at {
        ctx.say(format!("  created:    {t}"));
    }
    if let Some(audit_event_id) = &outcome.audit_event_id {
        ctx.say(format!("  audit_event_id: {audit_event_id}"));
    }
    ctx.say("");
    ctx.say("next:");
    if outcome.metadata.current == Some(true) {
        ctx.say(format!("  agentstack skill status {}", unversioned));
    } else {
        ctx.say("  candidate: approve before readers install it.");
        ctx.say(format!(
            "  agentstack skill version approve {}@{}",
            unversioned, outcome.metadata.version
        ));
    }
    ctx.say(format!("  agentstack skill version list {}", unversioned));
}

fn render_batch_table(ctx: &Ctx, plan: &BatchPlan) {
    if plan.rows.is_empty() {
        return;
    }
    let rows: Vec<(String, String, String)> = plan
        .rows
        .iter()
        .map(|row| (row.name.clone(), display_path(&row.path), batch_status(row)))
        .collect();
    let name_width = rows
        .iter()
        .map(|(name, _, _)| name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let path_width = rows
        .iter()
        .map(|(_, path, _)| path.len())
        .max()
        .unwrap_or(0)
        .max("PATH".len());

    ctx.say(format!(
        "{:<name_width$}  {:<path_width$}  STATUS",
        "NAME", "PATH"
    ));
    for (name, path, status) in rows {
        ctx.say(format!(
            "{name:<name_width$}  {path:<path_width$}  {status}"
        ));
    }
}

fn batch_status(row: &PlannedSkill) -> String {
    match &row.status {
        PlannedStatus::Selected if row.lint_warnings == 0 => "valid".to_string(),
        PlannedStatus::Selected if row.lint_warnings == 1 => "valid (1 lint warning)".to_string(),
        PlannedStatus::Selected => format!("valid ({} lint warnings)", row.lint_warnings),
        PlannedStatus::Invalid(reason) => format!("invalid: {reason}"),
        PlannedStatus::Excluded(reason) => (*reason).to_string(),
    }
}

fn render_batch_progress(
    ctx: Option<&Ctx>,
    index: usize,
    total: usize,
    row: &PlannedSkill,
    result: Result<&PushOutcome, &str>,
    dry_run: bool,
) {
    let Some(ctx) = ctx else {
        return;
    };
    if ctx.json {
        return;
    }
    match result {
        Ok(outcome) if dry_run => ctx.say(format!(
            "[{index}/{total}] {} ... local push plan for {} ok",
            row.name, outcome.skill_ref
        )),
        Ok(outcome) => ctx.say(format!(
            "[{index}/{total}] {} ... {} ok",
            row.name, outcome.version
        )),
        Err(reason) => ctx.say(format!(
            "[{index}/{total}] {} ... failed: {reason}",
            row.name
        )),
    }
}

fn render_batch_summary(ctx: &Ctx, outcome: &BatchOutcome) {
    ctx.say(format!(
        "pushed {} · skipped {} · failed {}",
        outcome.pushed.len(),
        outcome.skipped.len(),
        outcome.failed.len()
    ));
}

#[derive(Serialize)]
struct BatchJson<'a> {
    batch: bool,
    dry_run: bool,
    org: &'a str,
    path: String,
    pushed: Vec<BatchPushedJson<'a>>,
    skipped: Vec<BatchSkippedJson<'a>>,
    failed: Vec<BatchFailedJson<'a>>,
    summary: BatchSummary,
}

#[derive(Serialize)]
struct BatchPushedJson<'a> {
    name: &'a str,
    skill_ref: &'a str,
    version: &'a str,
    sha256: &'a str,
    size_bytes: u64,
    audit_event_id: Option<&'a str>,
}

#[derive(Serialize)]
struct BatchSkippedJson<'a> {
    name: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct BatchFailedJson<'a> {
    name: &'a str,
    reason: &'a str,
}

#[derive(Serialize)]
struct BatchSummary {
    pushed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    would_push: Option<usize>,
    skipped: usize,
    failed: usize,
}

fn render_batch_json(ctx: &Ctx, outcome: &BatchOutcome) -> Result<()> {
    let out = BatchJson {
        batch: true,
        dry_run: outcome.dry_run,
        org: &outcome.org,
        path: outcome.path.display().to_string(),
        pushed: outcome
            .pushed
            .iter()
            .map(|row| BatchPushedJson {
                name: &row.name,
                skill_ref: &row.skill_ref,
                version: &row.version,
                sha256: &row.sha256,
                size_bytes: row.size_bytes,
                audit_event_id: row.audit_event_id.as_deref(),
            })
            .collect(),
        skipped: outcome
            .skipped
            .iter()
            .map(|row| BatchSkippedJson {
                name: &row.name,
                reason: &row.reason,
            })
            .collect(),
        failed: outcome
            .failed
            .iter()
            .map(|row| BatchFailedJson {
                name: &row.name,
                reason: &row.reason,
            })
            .collect(),
        summary: BatchSummary {
            pushed: outcome.pushed.len(),
            would_push: outcome.dry_run.then_some(outcome.pushed.len()),
            skipped: outcome.skipped.len(),
            failed: outcome.failed.len(),
        },
    };
    ctx.say_always(serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn display_path(path: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(relative) = path.strip_prefix(cwd)
        && !relative.as_os_str().is_empty()
    {
        return format!("./{}", relative.display());
    }
    path.display().to_string()
}

fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let mut dp = vec![vec![false; name.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;

    for p in 1..=pattern.len() {
        if pattern[p - 1] == b'*' {
            dp[p][0] = dp[p - 1][0];
        }
    }

    for p in 1..=pattern.len() {
        for n in 1..=name.len() {
            dp[p][n] = match pattern[p - 1] {
                b'*' => dp[p - 1][n] || dp[p][n - 1],
                b'?' => dp[p - 1][n - 1],
                c => c == name[n - 1] && dp[p - 1][n - 1],
            };
        }
    }

    dp[pattern.len()][name.len()]
}
