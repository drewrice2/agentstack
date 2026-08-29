//! Installed update handlers for `skill update`, `stack update`, and receipt batch updates.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::configured_client_with_identity;
use super::diff::{FileChangeSummary, file_changes_between_dirs};
use super::install::{
    RemoteInstallOptions, StackInstallOptions, create_remote_stage_parent,
    run_remote_update_with_client, run_stack_with_client_unlocked, validate_stack_resolution,
};
use crate::cache::Cache;
use crate::config::ConfigStore;
use crate::error::CliError;
use crate::install::{AppliedOverlay, TargetInstallLock};
use crate::installed_scan::{
    InstalledRow, InstalledStackRow, scan_installed, scan_installed_stacks,
};
use crate::output::Ctx;
use crate::package::{PackageHash, PackageManifest, unpack_verified_bytes};
use crate::receipt::{
    InstallReceipt, ReceiptSourceType, STACK_RECEIPT_FILE, StackInstallReceipt,
    StackInstallReceiptItem, StackLookup, ensure_stack_receipt_dir_not_symlink, format_hash,
    installed_timestamp, read_receipt_from_dir, read_stack_receipt_file, receipt_path,
    remove_stack_referrer, stack_referrers, validate_stack_receipt_item_paths,
    write_receipt_to_dir, write_stack_receipt,
};
use crate::registry::{
    PullClientOptions, RegistryClient, RegistryUrl, StackResolve, StackResolvedItem, VersionInfo,
};
use crate::skill::check_slug;
use crate::skill_ref::SkillRef;
use crate::targets::{InstallTarget, TargetResolver};

pub struct Args {
    pub subject: Option<String>,
    pub subject_name: Option<String>,
    pub all: bool,
    pub target: Option<String>,
    pub check: bool,
    pub force: bool,
    pub prune: bool,
}

pub struct UpdateOptions<'a> {
    pub skill_name: &'a str,
    pub target: InstallTarget,
    pub target_root: &'a Path,
    pub registry_url: Option<&'a str>,
    pub check: bool,
    pub force: bool,
    pub json: bool,
    pub quiet: bool,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
}

pub struct StackUpdateOptions<'a> {
    pub stack: &'a str,
    pub target: InstallTarget,
    pub target_root: &'a Path,
    pub registry_url: Option<&'a str>,
    pub check: bool,
    pub force: bool,
    pub prune: bool,
    pub json: bool,
    pub quiet: bool,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
}

struct Decision {
    receipt: InstallReceipt,
    skill_ref: SkillRef,
    latest: VersionInfo,
    installed_version: Option<String>,
    installed_yanked: Option<YankedVersion>,
    update_available: bool,
    drift: super::install_receipts::ContentDrift,
}

#[derive(Debug, Clone)]
struct YankedVersion {
    version: String,
    reason: Option<String>,
}

#[derive(Serialize)]
struct UpdateJson<'a> {
    skill_name: &'a str,
    target: &'static str,
    source_ref: String,
    registry_url: Option<&'a str>,
    installed_version: Option<&'a str>,
    latest_version: &'a str,
    update_available: bool,
    installed_yanked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_yank_reason: Option<&'a str>,
    updated: bool,
    forced: bool,
    /// `true` if installed files drifted from the recorded package, `false` if
    /// they match, `null` when there is no recorded hash to compare against.
    content_drifted: Option<bool>,
    destination: Option<&'a Path>,
    receipt: Option<&'a Path>,
    cache_package: Option<&'a Path>,
    /// File-level preview of what the update would touch. Present only for
    /// `--check` when an update is available and the new archive downloaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<&'a FileChangeSummary>,
    /// Why the file change preview was unavailable, when the download or
    /// comparison failed during `--check`.
    #[serde(skip_serializing_if = "Option::is_none")]
    changes_error: Option<String>,
    overlay: Option<&'a AppliedOverlay>,
    next_command: Option<String>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.check && args.force {
        bail!("cannot combine `--check` and `--force`");
    }
    if args.all && args.prune {
        bail!("--prune is only valid with `agentstack stack update <name>`");
    }
    if args.all {
        return run_all(ctx, args);
    }

    let subject = parse_update_subject(args.subject.as_deref(), args.subject_name.as_deref())?;
    let target = args
        .target
        .context("missing `--target`; specify which target's installed copy to update")?;

    let target = InstallTarget::parse(&target)?;
    let store = ConfigStore::load().context("failed to load config")?;
    let resolver = TargetResolver::new(&store);
    let resolved = resolver.resolve(target)?;

    match subject {
        UpdateSubject::Skill(skill_name) => {
            if args.prune {
                bail!("--prune is only valid with `agentstack stack update <name>`");
            }
            check_slug(&skill_name)
                .map_err(|reason| anyhow::anyhow!("invalid skill name `{skill_name}`: {reason}"))?;
            reject_non_registry_receipt_before_auth(&skill_name, target, &resolved.path)?;
            let (configured, installed_by) = configured_client_with_identity(ctx)?;
            run_with_client(
                &configured.client,
                UpdateOptions {
                    skill_name: &skill_name,
                    target,
                    target_root: &resolved.path,
                    registry_url: Some(&configured.url),
                    check: args.check,
                    force: args.force,
                    json: ctx.json,
                    quiet: ctx.quiet,
                    installed_by,
                    cache_root: None,
                },
            )
        }
        UpdateSubject::Stack(stack) => {
            StackLookup::parse(&stack)?;
            let (configured, installed_by) = configured_client_with_identity(ctx)?;
            run_stack_update_with_client(
                &configured.client,
                StackUpdateOptions {
                    stack: &stack,
                    target,
                    target_root: &resolved.path,
                    registry_url: Some(&configured.url),
                    check: args.check,
                    force: args.force,
                    prune: args.prune,
                    json: ctx.json,
                    quiet: ctx.quiet,
                    installed_by,
                    cache_root: None,
                },
            )
            .map(|_| ())
        }
    }
}

fn reject_non_registry_receipt_before_auth(
    skill_name: &str,
    target: InstallTarget,
    target_root: &Path,
) -> Result<()> {
    let installed_path = target_root.join(skill_name);
    let Ok(receipt) = read_receipt_from_dir(&installed_path) else {
        return Ok(());
    };
    if receipt.source_type == ReceiptSourceType::Registry {
        return Ok(());
    }
    Err(non_registry_receipt_error(
        skill_name,
        &receipt.source_ref,
        target,
    ))
}

fn non_registry_receipt_error(
    skill_name: &str,
    source_ref: &str,
    target: InstallTarget,
) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot update `{name}` from a local install receipt (source: `{source}`, target: `{target}`); install from a registry ref first: agentstack skill install <org>/{name} --target {target} --force",
        name = skill_name,
        source = source_ref,
        target = target.as_str(),
    )
}

enum UpdateSubject {
    Skill(String),
    Stack(String),
}

fn parse_update_subject(
    subject: Option<&str>,
    subject_name: Option<&str>,
) -> Result<UpdateSubject> {
    match (subject, subject_name) {
        (Some("skill"), Some(name)) => Ok(UpdateSubject::Skill(name.to_string())),
        (Some("stack"), Some(name)) => Ok(UpdateSubject::Stack(name.to_string())),
        (Some("skill"), None) => {
            bail!("`agentstack skill update <skill>` requires a skill name")
        }
        (Some("stack"), None) => {
            bail!("`agentstack stack update <stack>` requires a stack name")
        }
        (Some(kind), Some(_)) => {
            bail!("unknown update kind `{kind}` (expected `skill` or `stack`)")
        }
        (Some(name), None) => Ok(UpdateSubject::Skill(name.to_string())),
        (None, None) => {
            bail!(
                "`agentstack install update` requires --all; use `agentstack skill update <skill> --target <target>` or `agentstack stack update <org>/<stack> --target <target>` for one resource"
            )
        }
        (None, Some(_)) => unreachable!("clap cannot provide a second positional without first"),
    }
}

fn run_all(ctx: &Ctx, args: Args) -> Result<()> {
    let target_filter = match args.target.as_deref() {
        Some(t) => Some(InstallTarget::parse(t)?),
        None => None,
    };

    let scanned = scan_installed(|receipt_file, e| {
        if !ctx.json {
            ctx.warn(format!(
                "warning: skipping unreadable install receipt `{}`: {e}",
                receipt_file.display()
            ));
        }
    })?;

    let rows = batch_rows_from_scanned(scanned, target_filter);
    let stack_hints = if rows.is_empty() {
        stack_update_hints(ctx, target_filter)?
    } else {
        Vec::new()
    };
    let (registry_rows, local_rows): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|row| row.receipt.source_type == ReceiptSourceType::Registry);

    let mut outcome = if registry_rows.is_empty() {
        BatchUpdateOutcome {
            target_filter,
            check: args.check,
            force: args.force,
            results: Vec::new(),
        }
    } else {
        let (configured, installed_by) = configured_client_with_identity(ctx)?;

        run_all_with_client(
            &configured.client,
            UpdateAllOptions {
                rows: registry_rows,
                target_filter,
                registry_url: Some(&configured.url),
                check: args.check,
                force: args.force,
                installed_by,
                cache_root: None,
            },
        )
    };
    outcome.results.extend(local_rows.into_iter().map(|row| {
        BatchUpdateRowOutcome {
            skill_name: row.skill_name,
            target: row.target,
            status: BatchUpdateRowStatus::Skipped {
                reason:
                    "local installs are not registry-updateable; install from <org>/<skill> first"
                        .to_string(),
            },
        }
    }));

    render_batch(ctx, &outcome, &stack_hints)?;

    if outcome.failed_count() > 0 {
        bail!(
            "update --all failed for {} skill{}",
            outcome.failed_count(),
            if outcome.failed_count() == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn batch_rows_from_scanned(
    scanned: Vec<InstalledRow>,
    target_filter: Option<InstallTarget>,
) -> Vec<BatchUpdateRow> {
    scanned
        .into_iter()
        .filter(|row| target_filter.is_none_or(|t| row.target == t))
        .filter(|row| stack_referrers(&row.receipt).is_empty())
        .map(|row| {
            let target_root = row
                .installed_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| row.installed_path.clone());
            let skill_name = row.receipt.skill_name.clone();
            BatchUpdateRow {
                target: row.target,
                target_root,
                installed_path: row.installed_path,
                receipt_path: row.receipt_path,
                skill_name,
                receipt: row.receipt,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
struct StackUpdateHint {
    target: InstallTarget,
    org: String,
    stack: String,
}

fn stack_update_hints(
    ctx: &Ctx,
    target_filter: Option<InstallTarget>,
) -> Result<Vec<StackUpdateHint>> {
    let stacks = scan_installed_stacks(|receipt_file, e| {
        if !ctx.json {
            ctx.warn(format!(
                "warning: skipping unreadable stack install receipt `{}`: {e}",
                receipt_file.display()
            ));
        }
    })?;
    Ok(stacks
        .into_iter()
        .filter(|row| target_filter.is_none_or(|t| row.target == t))
        .map(|row| StackUpdateHint {
            target: row.target,
            org: row.receipt.org,
            stack: row.receipt.stack,
        })
        .collect())
}

pub fn run_with_client(client: &dyn RegistryClient, opts: UpdateOptions<'_>) -> Result<()> {
    let work = decide_update(client, &opts)?;
    match work {
        UpdateWork::Check(decision) => {
            let preview = (opts.check && decision.update_available)
                .then(|| preview_file_changes(client, &opts, &decision));
            render_check(opts, &decision, preview.as_ref())
        }
        UpdateWork::Apply(decision) => {
            // An update is being applied. A drifted install only reaches this
            // point with --force (otherwise `decide_update_from_receipt`
            // refuses), so warn that the local modifications are about to be
            // overwritten. Legacy receipts without a content hash cannot be
            // checked for drift; say so instead of staying silent.
            if !opts.quiet && !opts.json {
                if decision.drift.is_drifted() {
                    eprintln!(
                        "warning: `{}` has local modifications that differ from the recorded package; --force overwrites them",
                        opts.skill_name
                    );
                } else if decision.drift == super::install_receipts::ContentDrift::Unknown {
                    eprintln!(
                        "warning: install receipt for `{}` has no recorded content hash; local-modification detection is unavailable for this receipt",
                        opts.skill_name
                    );
                }
            }
            let report = apply_update(client, &opts, &decision)?;
            render_updated(opts, &decision, &report)
        }
    }
}

#[derive(Debug, Clone)]
pub struct StackUpdateChange {
    pub skill: String,
    pub installed_version: String,
    pub resolved_version: String,
    pub installed_version_id: String,
    pub resolved_version_id: String,
    pub installed_hash: PackageHash,
    pub resolved_hash: PackageHash,
}

#[derive(Debug, Clone)]
pub struct StackUpdateOutcome {
    pub org: String,
    pub stack: String,
    pub target: InstallTarget,
    pub check: bool,
    pub force: bool,
    pub prune: bool,
    pub updated: bool,
    pub added: Vec<StackResolvedItem>,
    pub removed: Vec<StackInstallReceiptItem>,
    pub changed: Vec<StackUpdateChange>,
    pub unchanged: Vec<StackResolvedItem>,
    pub pruned: Vec<StackInstallReceiptItem>,
    pub detached: Vec<StackInstallReceiptItem>,
    pub stack_receipt_path: PathBuf,
    pub manifest_hash: PackageHash,
}

struct StackDecision {
    row: InstalledStackRow,
    resolved: StackResolve,
    diff: StackDiff,
}

struct StackDiff {
    added: Vec<StackResolvedItem>,
    removed: Vec<StackInstallReceiptItem>,
    changed: Vec<StackUpdateChange>,
    unchanged: Vec<StackResolvedItem>,
}

impl StackDiff {
    fn has_install_changes(&self) -> bool {
        !self.added.is_empty() || !self.changed.is_empty()
    }
}

pub fn run_stack_update_with_client(
    client: &dyn RegistryClient,
    opts: StackUpdateOptions<'_>,
) -> Result<StackUpdateOutcome> {
    let outcome = run_stack_update_quiet(client, &opts)?;
    render_stack_outcome(&opts, &outcome)?;
    Ok(outcome)
}

/// Decide and (unless `check`) apply a stack update without rendering, so
/// callers such as `sync` can aggregate the outcome into their own report.
pub(crate) fn run_stack_update_quiet(
    client: &dyn RegistryClient,
    opts: &StackUpdateOptions<'_>,
) -> Result<StackUpdateOutcome> {
    if opts.check {
        let decision = decide_stack_update(client, opts)?;
        return Ok(stack_outcome(
            opts,
            &decision,
            false,
            Vec::new(),
            Vec::new(),
            None,
        ));
    }

    let _lock = TargetInstallLock::acquire_for_target(
        opts.target_root,
        Some("update"),
        Some(opts.target.as_str()),
    )?;
    let decision = decide_stack_update(client, opts)?;
    apply_stack_update(client, opts, decision)
}

fn decide_stack_update(
    client: &dyn RegistryClient,
    opts: &StackUpdateOptions<'_>,
) -> Result<StackDecision> {
    let row = find_installed_stack_row(opts.target_root, opts.target, opts.stack)?;
    if row.receipt.kind != "stack" {
        bail!(
            "stack receipt `{}` has kind `{}`; expected `stack`",
            row.receipt_path.display(),
            row.receipt.kind
        );
    }
    if row.receipt.target != opts.target.as_str() {
        bail!(
            "stack install receipt target is `{}` but update was requested for `{}`",
            row.receipt.target,
            opts.target.as_str()
        );
    }
    ensure_same_registry_url(
        row.receipt.registry_url.as_deref(),
        opts.registry_url,
        opts.force,
        &format!("stack `{}/{}`", row.receipt.org, row.receipt.stack),
    )?;

    let resolved = client
        .resolve_stack(&row.receipt.org, &row.receipt.stack)
        .with_context(|| {
            format!(
                "resolve stack {}/{} failed",
                row.receipt.org, row.receipt.stack
            )
        })?;
    validate_stack_resolution(
        &resolved,
        &row.receipt.org,
        &row.receipt.stack,
        &format!(
            "installed stack `{}/{}`",
            row.receipt.org, row.receipt.stack
        ),
    )?;
    let diff = diff_stack_receipt(&row.receipt, &resolved)?;
    Ok(StackDecision {
        row,
        resolved,
        diff,
    })
}

fn apply_stack_update(
    client: &dyn RegistryClient,
    opts: &StackUpdateOptions<'_>,
    decision: StackDecision,
) -> Result<StackUpdateOutcome> {
    let needs_install = opts.force || decision.diff.has_install_changes();
    if !decision.diff.removed.is_empty() {
        for item in &decision.diff.removed {
            validate_stack_receipt_item_paths(opts.target_root, item)?;
        }
    }
    if opts.prune && !decision.diff.removed.is_empty() {
        preflight_stack_prune(opts, &decision.row.receipt, &decision.diff.removed)?;
    }

    if needs_install {
        let report = run_stack_with_client_unlocked(
            client,
            StackInstallOptions {
                org: &decision.row.receipt.org,
                stack: &decision.row.receipt.stack,
                dest_root: opts.target_root,
                target: opts.target.as_str(),
                force: opts.force,
                registry_url: opts.registry_url,
                installed_by: opts.installed_by.clone(),
                cache_root: opts.cache_root,
            },
        )
        .with_context(|| {
            format!(
                "failed to update stack `{}/{}`",
                decision.row.receipt.org, decision.row.receipt.stack
            )
        })?;

        let (pruned, detached) = prune_or_detach_removed(opts, &decision)?;
        return Ok(stack_outcome(
            opts,
            &decision,
            true,
            pruned,
            detached,
            Some(report.stack_receipt_path),
        ));
    }

    if decision.diff.removed.is_empty() {
        return Ok(stack_outcome(
            opts,
            &decision,
            false,
            Vec::new(),
            Vec::new(),
            None,
        ));
    }

    // A removal-only update (no installs/changes) still needs to prune or
    // detach the dropped members and rewrite the stack receipt; otherwise the
    // skill stays "owned" by a stack that no longer lists it (a wedge).
    let (pruned, detached) = prune_or_detach_removed(opts, &decision)?;
    let receipt = stack_receipt_from_resolved_paths(opts, &decision)?;
    let path = write_stack_receipt(opts.target_root, &receipt)?;
    Ok(stack_outcome(
        opts,
        &decision,
        true,
        pruned,
        detached,
        Some(path),
    ))
}

/// Removed stack members are either pruned (deleted) with `--prune` or
/// detached into standalone installs without it. Returns `(pruned, detached)`.
fn prune_or_detach_removed(
    opts: &StackUpdateOptions<'_>,
    decision: &StackDecision,
) -> Result<(Vec<StackInstallReceiptItem>, Vec<StackInstallReceiptItem>)> {
    if decision.diff.removed.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if opts.prune {
        let pruned = prune_stack_items(&decision.row.receipt, &decision.diff.removed)?;
        Ok((pruned, Vec::new()))
    } else {
        let detached = detach_removed_stack_items(&decision.row.receipt, &decision.diff.removed)?;
        Ok((Vec::new(), detached))
    }
}

fn stack_outcome(
    opts: &StackUpdateOptions<'_>,
    decision: &StackDecision,
    updated: bool,
    pruned: Vec<StackInstallReceiptItem>,
    detached: Vec<StackInstallReceiptItem>,
    stack_receipt_path: Option<PathBuf>,
) -> StackUpdateOutcome {
    StackUpdateOutcome {
        org: decision.row.receipt.org.clone(),
        stack: decision.row.receipt.stack.clone(),
        target: opts.target,
        check: opts.check,
        force: opts.force,
        prune: opts.prune,
        updated,
        added: decision.diff.added.clone(),
        removed: decision.diff.removed.clone(),
        changed: decision.diff.changed.clone(),
        unchanged: decision.diff.unchanged.clone(),
        pruned,
        detached,
        stack_receipt_path: stack_receipt_path.unwrap_or_else(|| decision.row.receipt_path.clone()),
        manifest_hash: decision.resolved.manifest_hash.clone(),
    }
}

fn find_installed_stack_row(
    target_root: &Path,
    target: InstallTarget,
    stack: &str,
) -> Result<InstalledStackRow> {
    let lookup = StackLookup::parse(stack)?;
    let install_ref = lookup.label_with_org_placeholder();
    let receipt_missing = || {
        stack_receipt_missing(
            format!(
                "no stack install receipt for `{stack}` in target `{}`",
                target.as_str()
            ),
            stack,
            &install_ref,
            target,
        )
    };
    let stacks_root = target_root.join(".agentstack-stacks");
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    if !stacks_root.exists() {
        return Err(stack_receipt_missing(
            format!(
                "no stack install receipts found in target `{}`; install the stack first",
                target.as_str()
            ),
            stack,
            &install_ref,
            target,
        )
        .into());
    }
    if !stacks_root.is_dir() {
        bail!(
            "stack receipt root `{}` exists but is not a directory",
            stacks_root.display()
        );
    }

    if let Some(org) = lookup.org.as_deref() {
        let org_path = stacks_root.join(org);
        ensure_stack_receipt_dir_not_symlink(&org_path)?;
        if !org_path.is_dir() {
            return Err(receipt_missing().into());
        }
        let stack_path = org_path.join(&lookup.stack);
        ensure_stack_receipt_dir_not_symlink(&stack_path)?;
        if !stack_path.is_dir() {
            return Err(receipt_missing().into());
        }
        let candidate = stack_path.join(STACK_RECEIPT_FILE);
        if !candidate.is_file() {
            return Err(receipt_missing().into());
        }
        let receipt = read_stack_receipt_file(&candidate)?;
        if receipt.org != org {
            return Err(receipt_missing().into());
        }
        return Ok(InstalledStackRow {
            target,
            target_root: target_root.to_path_buf(),
            receipt_path: candidate,
            receipt,
        });
    }

    let mut rows = Vec::new();
    for org_entry in fs::read_dir(&stacks_root)
        .with_context(|| format!("failed to read `{}`", stacks_root.display()))?
    {
        let org_entry = org_entry
            .with_context(|| format!("failed to read entry in `{}`", stacks_root.display()))?;
        let org_path = org_entry.path();
        ensure_stack_receipt_dir_not_symlink(&org_path)?;
        if !org_path.is_dir() {
            continue;
        }
        let stack_path = org_path.join(&lookup.stack);
        ensure_stack_receipt_dir_not_symlink(&stack_path)?;
        if !stack_path.is_dir() {
            continue;
        }
        let candidate = stack_path.join(STACK_RECEIPT_FILE);
        if !candidate.is_file() {
            continue;
        }
        let receipt = read_stack_receipt_file(&candidate)?;
        rows.push(InstalledStackRow {
            target,
            target_root: target_root.to_path_buf(),
            receipt_path: candidate,
            receipt,
        });
    }

    match rows.len() {
        0 => Err(receipt_missing().into()),
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => bail!(
            "multiple stack install receipts named `{stack}` found in target `{}`; remove the duplicate receipt or use a unique stack slug",
            target.as_str()
        ),
    }
}

fn stack_receipt_missing(
    message: String,
    stack: &str,
    install_ref: &str,
    target: InstallTarget,
) -> CliError {
    CliError::new("install_receipt_missing", message)
        .resource(stack)
        .action("update")
        .next_command(format!(
            "agentstack stack install {install_ref} --target {}",
            target.as_str()
        ))
}

fn diff_stack_receipt(receipt: &StackInstallReceipt, resolved: &StackResolve) -> Result<StackDiff> {
    let mut installed = BTreeMap::new();
    for item in &receipt.items {
        if installed.insert(item.skill.clone(), item).is_some() {
            bail!(
                "stack receipt `{}/{}` contains duplicate skill `{}`",
                receipt.org,
                receipt.stack,
                item.skill
            );
        }
    }

    let mut resolved_skills = BTreeSet::new();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for item in &resolved.items {
        // `validate_stack_resolution` already rejected duplicate
        // resolved skills before this diff runs.
        resolved_skills.insert(item.skill.clone());
        match installed.get(&item.skill) {
            None => added.push(item.clone()),
            Some(old) if stack_item_changed(old, item) => changed.push(StackUpdateChange {
                skill: item.skill.clone(),
                installed_version: old.version.clone(),
                resolved_version: item.version.clone(),
                installed_version_id: old.version_id.clone(),
                resolved_version_id: item.version_id.clone(),
                installed_hash: old.archive_hash.clone(),
                resolved_hash: item.archive_hash.clone(),
            }),
            Some(_) => unchanged.push(item.clone()),
        }
    }

    let removed = receipt
        .items
        .iter()
        .filter(|item| !resolved_skills.contains(&item.skill))
        .cloned()
        .collect();

    Ok(StackDiff {
        added,
        removed,
        changed,
        unchanged,
    })
}

fn stack_item_changed(old: &StackInstallReceiptItem, new: &StackResolvedItem) -> bool {
    old.version_id != new.version_id
        || old.version != new.version
        || old.archive_hash != new.archive_hash
}

fn preflight_stack_prune(
    opts: &StackUpdateOptions<'_>,
    stack_receipt: &StackInstallReceipt,
    removed: &[StackInstallReceiptItem],
) -> Result<()> {
    for item in removed {
        let child_receipt = match fs::metadata(&item.installed_receipt_path) {
            Ok(_) => Some(read_receipt_from_dir(&item.install_path).with_context(|| {
                format!(
                    "failed to read install receipt for pruned stack child `{}`",
                    item.skill
                )
            })?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to stat install receipt `{}`",
                        item.installed_receipt_path.display()
                    )
                });
            }
        };

        let Some(child_receipt) = child_receipt else {
            if !item.install_path.exists() || opts.force {
                continue;
            }
            bail!(
                "refusing to prune `{}` because its install receipt is missing; rerun with --force if this path is safe to remove",
                item.skill
            );
        };

        if stack_prune_owns_child(opts, stack_receipt, item, &child_receipt) || opts.force {
            continue;
        }

        let owner = child_receipt_owner_label(&child_receipt);
        bail!(
            "refusing to prune `{}` because it belongs to {owner}; rerun with --force to remove it anyway",
            item.skill
        );
    }
    Ok(())
}

fn stack_prune_owns_child(
    opts: &StackUpdateOptions<'_>,
    stack_receipt: &StackInstallReceipt,
    item: &StackInstallReceiptItem,
    child_receipt: &InstallReceipt,
) -> bool {
    child_receipt.target == opts.target.as_str()
        && child_receipt.installed_path == item.install_path
        && stack_referrers(child_receipt)
            .iter()
            .any(|via| via.org == stack_receipt.org && via.stack == stack_receipt.stack)
}

fn child_receipt_owner_label(receipt: &InstallReceipt) -> String {
    let refs = stack_referrers(receipt);
    if !refs.is_empty() {
        return format!(
            "stack(s) {}",
            refs.iter()
                .map(|via| format!("`{}/{}`", via.org, via.stack))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match &receipt.installed_via {
        Some(via) => format!("{} provenance", via.kind),
        None => "an independent install".to_string(),
    }
}

pub(crate) fn prune_stack_items(
    stack_receipt: &StackInstallReceipt,
    removed: &[StackInstallReceiptItem],
) -> Result<Vec<StackInstallReceiptItem>> {
    let mut pruned = Vec::new();
    for item in removed {
        if let Ok(mut child_receipt) = read_receipt_from_dir(&item.install_path) {
            let mut refs = stack_referrers(&child_receipt);
            if refs.len() > 1 {
                remove_stack_referrer(&mut refs, &stack_receipt.org, &stack_receipt.stack);
                child_receipt.installed_via_stacks = refs;
                child_receipt.installed_via = child_receipt.installed_via_stacks.first().cloned();
                write_receipt_to_dir(&item.install_path, &child_receipt)?;
                continue;
            }
        }
        match fs::symlink_metadata(&item.install_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "refusing to prune `{}` because `{}` is a symlink",
                        item.skill,
                        item.install_path.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!(
                        "refusing to prune `{}` because `{}` is not a directory",
                        item.skill,
                        item.install_path.display()
                    );
                }
                fs::remove_dir_all(&item.install_path).with_context(|| {
                    format!("failed to remove `{}`", item.install_path.display())
                })?;
                pruned.push(item.clone());
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                pruned.push(item.clone());
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to stat `{}`", item.install_path.display()));
            }
        }
    }
    Ok(pruned)
}

/// Detach members dropped from a stack definition (non-prune update) into
/// standalone installs: clear this stack from each child receipt's referrers
/// while keeping the files on disk. Returns the items that were detached.
fn detach_removed_stack_items(
    stack_receipt: &StackInstallReceipt,
    removed: &[StackInstallReceiptItem],
) -> Result<Vec<StackInstallReceiptItem>> {
    let mut detached = Vec::new();
    for item in removed {
        let Ok(mut child_receipt) = read_receipt_from_dir(&item.install_path) else {
            continue;
        };
        let mut refs = stack_referrers(&child_receipt);
        remove_stack_referrer(&mut refs, &stack_receipt.org, &stack_receipt.stack);
        child_receipt.installed_via_stacks = refs;
        child_receipt.installed_via = child_receipt.installed_via_stacks.first().cloned();
        write_receipt_to_dir(&item.install_path, &child_receipt)?;
        detached.push(item.clone());
    }
    Ok(detached)
}

fn stack_receipt_from_resolved_paths(
    opts: &StackUpdateOptions<'_>,
    decision: &StackDecision,
) -> Result<StackInstallReceipt> {
    let installed_by_skill: BTreeMap<&str, &StackInstallReceiptItem> = decision
        .row
        .receipt
        .items
        .iter()
        .map(|item| (item.skill.as_str(), item))
        .collect();
    let items = decision
        .resolved
        .items
        .iter()
        .map(|item| {
            let installed = installed_by_skill.get(item.skill.as_str());
            let install_path = installed
                .map(|old| old.install_path.clone())
                .unwrap_or_else(|| opts.target_root.join(&item.skill));
            let installed_receipt_path = installed
                .map(|old| old.installed_receipt_path.clone())
                .unwrap_or_else(|| receipt_path(&install_path));
            StackInstallReceiptItem {
                skill: item.skill.clone(),
                version_id: item.version_id.clone(),
                version: item.version.clone(),
                archive_hash: item.archive_hash.clone(),
                install_path,
                installed_receipt_path,
            }
        })
        .collect();

    Ok(StackInstallReceipt {
        schema_version: crate::receipt::RECEIPT_SCHEMA_VERSION,
        kind: "stack".to_string(),
        org: decision.resolved.stack.org.clone(),
        stack: decision.resolved.stack.slug.clone(),
        registry_url: opts.registry_url.map(str::to_string),
        visibility: decision.resolved.stack.visibility,
        team: decision.resolved.stack.team.clone(),
        resolved_at: decision.resolved.resolved_at.clone(),
        manifest_hash: decision.resolved.manifest_hash.clone(),
        target: opts.target.as_str().to_string(),
        installed_at: installed_timestamp()?,
        installed_by: opts.installed_by.clone(),
        items,
    })
}

#[derive(Serialize)]
struct StackUpdateJson<'a> {
    kind: &'static str,
    org: &'a str,
    stack: &'a str,
    target: &'static str,
    registry_url: Option<&'a str>,
    check: bool,
    force: bool,
    prune: bool,
    updated: bool,
    manifest_hash: &'a PackageHash,
    stack_receipt: &'a Path,
    added: Vec<StackResolvedItemJson<'a>>,
    removed: Vec<StackReceiptItemJson<'a>>,
    changed: Vec<StackChangeJson<'a>>,
    unchanged: usize,
    pruned: Vec<StackReceiptItemJson<'a>>,
    detached: Vec<StackReceiptItemJson<'a>>,
    next_command: Option<String>,
}

#[derive(Serialize)]
struct StackResolvedItemJson<'a> {
    skill: &'a str,
    version: &'a str,
    version_id: &'a str,
    archive_hash: &'a PackageHash,
    version_policy: &'static str,
}

#[derive(Serialize)]
struct StackReceiptItemJson<'a> {
    skill: &'a str,
    version: &'a str,
    version_id: &'a str,
    archive_hash: &'a PackageHash,
    install_path: &'a Path,
    installed_receipt_path: &'a Path,
}

#[derive(Serialize)]
struct StackChangeJson<'a> {
    skill: &'a str,
    installed_version: &'a str,
    resolved_version: &'a str,
    installed_version_id: &'a str,
    resolved_version_id: &'a str,
    installed_hash: &'a PackageHash,
    resolved_hash: &'a PackageHash,
}

fn render_stack_outcome(opts: &StackUpdateOptions<'_>, outcome: &StackUpdateOutcome) -> Result<()> {
    if opts.json {
        let next_command = stack_next_command(outcome);
        let payload = StackUpdateJson {
            kind: "stack",
            org: &outcome.org,
            stack: &outcome.stack,
            target: outcome.target.as_str(),
            registry_url: opts.registry_url,
            check: outcome.check,
            force: outcome.force,
            prune: outcome.prune,
            updated: outcome.updated,
            manifest_hash: &outcome.manifest_hash,
            stack_receipt: &outcome.stack_receipt_path,
            added: outcome.added.iter().map(stack_resolved_item_json).collect(),
            removed: outcome
                .removed
                .iter()
                .map(stack_receipt_item_json)
                .collect(),
            changed: outcome.changed.iter().map(stack_change_json).collect(),
            unchanged: outcome.unchanged.len(),
            pruned: outcome.pruned.iter().map(stack_receipt_item_json).collect(),
            detached: outcome
                .detached
                .iter()
                .map(stack_receipt_item_json)
                .collect(),
            next_command,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if outcome.check {
        render_stack_check_human(opts, outcome);
    } else {
        render_stack_apply_human(opts, outcome);
    }
    Ok(())
}

fn stack_resolved_item_json(item: &StackResolvedItem) -> StackResolvedItemJson<'_> {
    StackResolvedItemJson {
        skill: &item.skill,
        version: &item.version,
        version_id: &item.version_id,
        archive_hash: &item.archive_hash,
        version_policy: item.version_policy.as_str(),
    }
}

fn stack_receipt_item_json(item: &StackInstallReceiptItem) -> StackReceiptItemJson<'_> {
    StackReceiptItemJson {
        skill: &item.skill,
        version: &item.version,
        version_id: &item.version_id,
        archive_hash: &item.archive_hash,
        install_path: &item.install_path,
        installed_receipt_path: &item.installed_receipt_path,
    }
}

fn stack_change_json(change: &StackUpdateChange) -> StackChangeJson<'_> {
    StackChangeJson {
        skill: &change.skill,
        installed_version: &change.installed_version,
        resolved_version: &change.resolved_version,
        installed_version_id: &change.installed_version_id,
        resolved_version_id: &change.resolved_version_id,
        installed_hash: &change.installed_hash,
        resolved_hash: &change.resolved_hash,
    }
}

fn render_stack_check_human(opts: &StackUpdateOptions<'_>, outcome: &StackUpdateOutcome) {
    if !stack_outcome_has_reportable_changes(outcome) {
        println!(
            "stack `{}/{}` is up to date ({})",
            outcome.org,
            outcome.stack,
            format_hash(&outcome.manifest_hash)
        );
        if !opts.quiet {
            println!(
                "next: agentstack stack show {}/{} --target {}",
                outcome.org,
                outcome.stack,
                outcome.target.as_str()
            );
        }
        return;
    }

    println!(
        "stack update available for `{}/{}`",
        outcome.org, outcome.stack
    );
    render_stack_change_lines(outcome);
    if !opts.quiet {
        println!();
        println!("next:");
        if let Some(primary) = stack_next_command(outcome) {
            println!("  {primary}");
            if !outcome.removed.is_empty() && !primary.ends_with(" --prune") {
                println!("  {primary} --prune");
            }
        }
    }
}

fn render_stack_apply_human(opts: &StackUpdateOptions<'_>, outcome: &StackUpdateOutcome) {
    if outcome.updated {
        println!("updated stack `{}/{}`", outcome.org, outcome.stack);
    } else {
        println!(
            "stack `{}/{}` is up to date ({})",
            outcome.org,
            outcome.stack,
            format_hash(&outcome.manifest_hash)
        );
    }
    render_stack_change_lines(outcome);
    if outcome.force && !outcome.updated {
        println!("  forced: no write was needed");
    }
    if opts.quiet {
        return;
    }
    println!("  target: {}", outcome.target.as_str());
    println!("  receipt: {}", outcome.stack_receipt_path.display());
    if !outcome.detached.is_empty() {
        println!(
            "  {} removed skill(s) detached as standalone installs (rerun with --prune to delete them instead)",
            outcome.detached.len()
        );
    }
    println!();
    println!(
        "next: agentstack stack show {}/{} --target {}",
        outcome.org,
        outcome.stack,
        outcome.target.as_str()
    );
}

fn render_stack_change_lines(outcome: &StackUpdateOutcome) {
    let pruned: HashSet<&str> = outcome
        .pruned
        .iter()
        .map(|item| item.skill.as_str())
        .collect();
    let detached: HashSet<&str> = outcome
        .detached
        .iter()
        .map(|item| item.skill.as_str())
        .collect();
    for item in &outcome.added {
        println!("  added:   {}@{}", item.skill, item.version);
    }
    for change in &outcome.changed {
        println!(
            "  updated: {} {} -> {}",
            change.skill, change.installed_version, change.resolved_version
        );
    }
    for item in &outcome.removed {
        let suffix = if pruned.contains(item.skill.as_str()) {
            "pruned"
        } else if detached.contains(item.skill.as_str()) {
            "detached"
        } else {
            "kept"
        };
        println!("  removed: {}@{} ({suffix})", item.skill, item.version);
    }
}

fn stack_outcome_has_reportable_changes(outcome: &StackUpdateOutcome) -> bool {
    !outcome.added.is_empty() || !outcome.changed.is_empty() || !outcome.removed.is_empty()
}

fn stack_next_command(outcome: &StackUpdateOutcome) -> Option<String> {
    if !outcome.check || !stack_outcome_has_reportable_changes(outcome) {
        return None;
    }
    if outcome.added.is_empty() && outcome.changed.is_empty() {
        return Some(format!(
            "agentstack stack update {}/{} --target {} --prune",
            outcome.org,
            outcome.stack,
            outcome.target.as_str()
        ));
    }
    Some(format!(
        "agentstack stack update {}/{} --target {}",
        outcome.org,
        outcome.stack,
        outcome.target.as_str()
    ))
}

enum UpdateWork {
    Check(Decision),
    Apply(Decision),
}

fn decide_update(client: &dyn RegistryClient, opts: &UpdateOptions<'_>) -> Result<UpdateWork> {
    let installed_path = opts.target_root.join(opts.skill_name);
    let receipt = read_receipt_from_dir(&installed_path).map_err(|_| {
        CliError::new(
            "install_receipt_missing",
            format!("no install receipt for `{}`", installed_path.display()),
        )
        .resource(installed_path.display().to_string())
        .action("update")
        .next_command(format!(
            "agentstack install list --target {}",
            opts.target.as_str()
        ))
    })?;
    decide_update_from_receipt(client, opts, receipt)
}

fn decide_update_from_receipt(
    client: &dyn RegistryClient,
    opts: &UpdateOptions<'_>,
    receipt: InstallReceipt,
) -> Result<UpdateWork> {
    let installed_path = opts.target_root.join(opts.skill_name);
    if receipt.source_type != ReceiptSourceType::Registry {
        return Err(non_registry_receipt_error(
            opts.skill_name,
            &receipt.source_ref,
            opts.target,
        ));
    }
    if receipt.target != opts.target.as_str() {
        bail!(
            "install receipt target is `{}` but update was requested for `{}`",
            receipt.target,
            opts.target.as_str()
        );
    }
    ensure_direct_skill_receipt(&receipt, opts.skill_name, opts.target)?;
    ensure_same_registry(&receipt, opts.registry_url, opts.force, opts.skill_name)?;

    let skill_ref = skill_ref_from_receipt(&receipt)?;
    let versions = client
        .list_versions(&skill_ref)
        .with_context(|| match opts.registry_url {
            Some(url) => format!("versions request to {url} failed"),
            None => "versions request failed".to_string(),
        })?;
    let latest = versions
        .iter()
        .find(|version| version.current.unwrap_or(false))
        .cloned()
        .with_context(|| {
            format!(
                "`{}` has uploaded candidate versions but no approved/current version yet; ask an org or team admin to run `agentstack skill version approve {}@<VERSION>`",
                skill_ref.unversioned(),
                skill_ref.unversioned()
            )
        })?;
    if latest.yanked_at.is_some() {
        let reason = latest.yank_reason.as_deref().unwrap_or("yanked");
        bail!(
            "approved/current version `{}/{}@{}` was yanked: {reason}; ask an org or team admin to approve a replacement before running update",
            skill_ref.org,
            skill_ref.name,
            latest.version,
        );
    }
    let installed_version = receipt.version.clone();
    let installed_yanked = installed_version.as_deref().and_then(|installed| {
        versions
            .iter()
            .find(|version| version.version == installed && version.yanked_at.is_some())
            .map(|version| YankedVersion {
                version: version.version.clone(),
                reason: version.yank_reason.clone(),
            })
    });
    let installed_hash = receipt.hash.clone();
    let latest_hash = format_hash(&latest.hash);
    let update_available = installed_version.as_deref() != Some(latest.version.as_str())
        || installed_hash.as_deref() != Some(latest_hash.as_str());
    let drift = super::install_receipts::content_drift(&installed_path, &receipt);
    let decision = Decision {
        receipt,
        skill_ref,
        latest,
        installed_version,
        installed_yanked,
        update_available,
        drift,
    };
    if opts.check {
        return Ok(UpdateWork::Check(decision));
    }
    if opts.force {
        return Ok(UpdateWork::Apply(decision));
    }
    if !decision.update_available {
        // Already up to date. If the installed files drifted, do NOT silently
        // no-op (which would leave the local edits unchanged but unreported);
        // surface the drift via the Check path instead. Without drift this is
        // the normal "no update available" no-op.
        return Ok(UpdateWork::Check(decision));
    }
    // An update is available, but overwriting local modifications requires an
    // explicit --force; refuse instead of silently discarding the edits.
    if decision.drift.is_drifted() {
        return Err(content_drift_refusal(opts.skill_name, opts.target).into());
    }
    Ok(UpdateWork::Apply(decision))
}

/// Refusal raised when an update would overwrite locally modified installed
/// files and the user did not pass `--force`.
fn content_drift_refusal(skill_name: &str, target: InstallTarget) -> CliError {
    let diff_command = format!(
        "agentstack skill diff {skill_name} --target {}",
        target.as_str()
    );
    CliError::new(
        "install_content_drifted",
        format!(
            "refusing to update `{skill_name}`: installed files have local modifications; review them with `{diff_command}`, or rerun with --force to overwrite them"
        ),
    )
    .resource(skill_name)
    .action("update")
    .next_command(diff_command)
}

/// Callers must have already rejected non-registry receipts before this runs.
fn ensure_same_registry(
    receipt: &InstallReceipt,
    effective_registry_url: Option<&str>,
    force: bool,
    skill_name: &str,
) -> Result<()> {
    ensure_same_registry_url(
        receipt.registry_url.as_deref(),
        effective_registry_url,
        force,
        &format!("`{skill_name}`"),
    )
}

pub(crate) fn ensure_same_registry_url(
    installed_registry_url: Option<&str>,
    effective_registry_url: Option<&str>,
    force: bool,
    description: &str,
) -> Result<()> {
    let Some(effective) = effective_registry_url else {
        return Ok(());
    };
    let Some(installed) = installed_registry_url else {
        if force {
            return Ok(());
        }
        bail!(
            "refusing to update {description} because its install receipt does not record a registry URL. rerun with `--force` to replace its provenance."
        );
    };
    if same_registry_base(installed, effective) {
        return Ok(());
    }
    if force {
        return Ok(());
    }
    bail!(
        "refusing to update {description} from a different registry: install receipt records `{installed}`, but the active registry is `{effective}`. rerun with `--force` to replace its provenance, or `agentstack registry use {installed}` to update from the original registry."
    )
}

pub(crate) fn same_registry_base(a: &str, b: &str) -> bool {
    match (RegistryUrl::parse(a), RegistryUrl::parse(b)) {
        (Ok(au), Ok(bu)) => au.normalized_base() == bu.normalized_base(),
        _ => a == b,
    }
}

fn ensure_direct_skill_receipt(
    receipt: &InstallReceipt,
    skill_name: &str,
    target: InstallTarget,
) -> Result<()> {
    let refs = stack_referrers(receipt);
    if !refs.is_empty() {
        let stacks = refs
            .iter()
            .map(|via| format!("`{}/{}`", via.org, via.stack))
            .collect::<Vec<_>>()
            .join(", ");
        let command = if refs.len() == 1 {
            format!(
                "agentstack stack update {}/{} --target {}",
                refs[0].org,
                refs[0].stack,
                target.as_str()
            )
        } else {
            format!(
                "agentstack stack update <org>/<stack> --target {}",
                target.as_str()
            )
        };
        bail!(
            "cannot update stack-owned child skill `{skill_name}` directly; it is referenced by {stacks}; run `{command}`"
        );
    }
    let Some(via) = &receipt.installed_via else {
        return Ok(());
    };
    bail!(
        "cannot update `{skill_name}` directly because its install receipt was written by `{}` provenance",
        via.kind
    )
}

fn apply_update(
    client: &dyn RegistryClient,
    opts: &UpdateOptions<'_>,
    decision: &Decision,
) -> Result<super::install::RemoteInstallReport> {
    let pinned = decision
        .skill_ref
        .clone()
        .with_version(decision.latest.version.clone())?;
    run_remote_update_with_client(
        client,
        RemoteInstallOptions {
            skill_ref: &pinned,
            dest_root: opts.target_root,
            target: opts.target.as_str(),
            force: opts.force,
            registry_url: opts.registry_url,
            installed_by: opts.installed_by.clone(),
            cache_root: opts.cache_root,
            allow_yanked: false,
        },
    )
    .with_context(|| format!("failed to update `{}`", opts.skill_name))
}

/// Download the new current version into the cache and compare its files
/// against the installed copy. Used by `--check` to preview what an update
/// would touch; callers degrade gracefully when this fails.
fn preview_file_changes(
    client: &dyn RegistryClient,
    opts: &UpdateOptions<'_>,
    decision: &Decision,
) -> Result<FileChangeSummary> {
    let pinned = decision
        .skill_ref
        .clone()
        .with_version(decision.latest.version.clone())?;
    let response = client
        .pull_with_options(
            &pinned,
            PullClientOptions {
                allow_yanked: false,
            },
        )
        .with_context(|| match opts.registry_url {
            Some(url) => format!("download from {url} failed"),
            None => "registry download failed".to_string(),
        })?;
    let actual = PackageHash::sha256_of(&response.archive);
    if actual != response.metadata.hash {
        bail!(
            "hash mismatch for {}: expected {} but archive bytes hash to {}",
            response.metadata.skill_ref(),
            response.metadata.hash.hex,
            actual.hex,
        );
    }

    let cache = match opts.cache_root {
        Some(root) => Cache::at(root.to_path_buf()),
        None => Cache::from_config().context("failed to open cache")?,
    };
    let stage_parent = create_remote_stage_parent(&cache, &response.metadata.name)?;
    let result = (|| {
        let unpacked = unpack_verified_bytes(&response.archive, &stage_parent, false, actual)
            .context("failed to unpack registry archive for preview")?;
        let cache_manifest = PackageManifest {
            name: response.metadata.name.clone(),
            description: response.metadata.description.clone(),
            version: response.metadata.version.clone(),
            files: unpacked.manifest.files.clone(),
        };
        cache.add_archive(
            cache_manifest,
            response.metadata.hash.clone(),
            &response.archive,
        )?;
        file_changes_between_dirs(
            &opts.target_root.join(opts.skill_name),
            &unpacked.out_path,
            &unpacked.manifest,
            opts.target.platform(),
        )
    })();
    let _ = fs::remove_dir_all(&stage_parent);
    result
}

/// One install receipt to consider in a batch update.
#[derive(Debug, Clone)]
pub struct BatchUpdateRow {
    pub target: InstallTarget,
    pub target_root: PathBuf,
    pub installed_path: PathBuf,
    pub receipt_path: PathBuf,
    pub skill_name: String,
    pub receipt: InstallReceipt,
}

pub struct UpdateAllOptions<'a> {
    pub rows: Vec<BatchUpdateRow>,
    pub target_filter: Option<InstallTarget>,
    pub registry_url: Option<&'a str>,
    pub check: bool,
    pub force: bool,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
}

#[derive(Debug, Clone)]
pub struct BatchUpdateRowOutcome {
    pub skill_name: String,
    pub target: InstallTarget,
    pub status: BatchUpdateRowStatus,
}

#[derive(Debug, Clone)]
pub enum BatchUpdateRowStatus {
    AlreadyCurrent {
        version: String,
    },
    UpdateAvailable {
        installed_version: Option<String>,
        latest_version: String,
        installed_yanked: Option<String>,
        /// File-level preview computed during `--check`; `None` when the
        /// preview download failed or no preview was requested.
        changes: Option<FileChangeSummary>,
    },
    Updated {
        installed_version: Option<String>,
        latest_version: String,
        forced: bool,
        content_drifted: bool,
    },
    Failed {
        reason: String,
    },
    Skipped {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct BatchUpdateOutcome {
    pub target_filter: Option<InstallTarget>,
    pub check: bool,
    pub force: bool,
    pub results: Vec<BatchUpdateRowOutcome>,
}

impl BatchUpdateOutcome {
    fn count_where(&self, f: impl Fn(&BatchUpdateRowStatus) -> bool) -> usize {
        self.results.iter().filter(|r| f(&r.status)).count()
    }

    pub fn updated_count(&self) -> usize {
        self.count_where(|s| matches!(s, BatchUpdateRowStatus::Updated { .. }))
    }

    pub fn already_current_count(&self) -> usize {
        self.count_where(|s| matches!(s, BatchUpdateRowStatus::AlreadyCurrent { .. }))
    }

    pub fn update_available_count(&self) -> usize {
        self.count_where(|s| matches!(s, BatchUpdateRowStatus::UpdateAvailable { .. }))
    }

    pub fn failed_count(&self) -> usize {
        self.count_where(|s| matches!(s, BatchUpdateRowStatus::Failed { .. }))
    }

    pub fn skipped_count(&self) -> usize {
        self.count_where(|s| matches!(s, BatchUpdateRowStatus::Skipped { .. }))
    }
}

pub fn run_all_with_client(
    client: &dyn RegistryClient,
    opts: UpdateAllOptions<'_>,
) -> BatchUpdateOutcome {
    let mut results = Vec::with_capacity(opts.rows.len());
    for row in &opts.rows {
        if opts
            .target_filter
            .is_some_and(|target| row.target != target)
        {
            continue;
        }
        let one = UpdateOptions {
            skill_name: &row.skill_name,
            target: row.target,
            target_root: &row.target_root,
            registry_url: opts.registry_url,
            check: opts.check,
            force: opts.force,
            json: false,
            quiet: true,
            installed_by: opts.installed_by.clone(),
            cache_root: opts.cache_root,
        };
        let status = run_one_for_batch(client, &one, row);
        results.push(BatchUpdateRowOutcome {
            skill_name: row.skill_name.clone(),
            target: row.target,
            status,
        });
    }
    BatchUpdateOutcome {
        target_filter: opts.target_filter,
        check: opts.check,
        force: opts.force,
        results,
    }
}

fn run_one_for_batch(
    client: &dyn RegistryClient,
    opts: &UpdateOptions<'_>,
    row: &BatchUpdateRow,
) -> BatchUpdateRowStatus {
    if let Err(e) = check_batch_row_paths(opts, row) {
        return BatchUpdateRowStatus::Failed {
            reason: format!("{e:#}"),
        };
    }
    let work = match decide_update_from_receipt(client, opts, row.receipt.clone()) {
        Ok(w) => w,
        Err(e) => {
            return BatchUpdateRowStatus::Failed {
                reason: format!("{e:#}"),
            };
        }
    };
    match work {
        UpdateWork::Check(decision) => {
            if decision.update_available {
                // Degrade gracefully: a failed preview download still reports
                // the version delta, just without file-level changes.
                let changes = opts
                    .check
                    .then(|| preview_file_changes(client, opts, &decision).ok())
                    .flatten();
                BatchUpdateRowStatus::UpdateAvailable {
                    installed_version: decision.installed_version,
                    latest_version: decision.latest.version,
                    installed_yanked: decision
                        .installed_yanked
                        .map(|yanked| yanked.reason.unwrap_or_else(|| "yanked".to_string())),
                    changes,
                }
            } else {
                BatchUpdateRowStatus::AlreadyCurrent {
                    version: decision.latest.version,
                }
            }
        }
        UpdateWork::Apply(decision) => {
            let forced = opts.force && !decision.update_available;
            // Mirror the single-skill path: record when the update overwrote local
            // modifications so `--all` does not silently clobber edits.
            let content_drifted = decision.drift.is_drifted();
            match apply_update(client, opts, &decision) {
                Ok(report) => BatchUpdateRowStatus::Updated {
                    installed_version: decision.installed_version,
                    latest_version: report.metadata.version,
                    forced,
                    content_drifted,
                },
                Err(e) => BatchUpdateRowStatus::Failed {
                    reason: format!("{e:#}"),
                },
            }
        }
    }
}

/// Guard against malformed scanned rows: the row's recorded paths must match
/// where this skill would actually live under its target root.
fn check_batch_row_paths(opts: &UpdateOptions<'_>, row: &BatchUpdateRow) -> Result<()> {
    let expected_path = opts.target_root.join(opts.skill_name);
    if row.installed_path != expected_path {
        bail!(
            "batch update row for `{}` points at `{}` but expected `{}`",
            opts.skill_name,
            row.installed_path.display(),
            expected_path.display()
        );
    }
    let expected_receipt_path = receipt_path(&row.installed_path);
    if row.receipt_path != expected_receipt_path {
        bail!(
            "batch update receipt for `{}` points at `{}` but expected `{}`",
            opts.skill_name,
            row.receipt_path.display(),
            expected_receipt_path.display()
        );
    }
    Ok(())
}

fn skill_ref_from_receipt(receipt: &InstallReceipt) -> Result<SkillRef> {
    if let Some(org) = &receipt.org {
        return Ok(SkillRef::new(org.clone(), receipt.skill_name.clone())?);
    }
    Ok(SkillRef::parse(&receipt.source_ref)?)
}

fn append_yanked_detail(detail: String, installed_yanked: Option<&str>) -> String {
    match installed_yanked {
        Some(reason) => format!("{detail} (installed version yanked: {reason})"),
        None => detail,
    }
}

fn append_changes_detail(detail: String, changes: Option<&FileChangeSummary>) -> String {
    match changes {
        Some(changes) => format!(
            "{detail} (+{} -{} ~{} files)",
            changes.added.len(),
            changes.removed.len(),
            changes.changed.len()
        ),
        None => detail,
    }
}

fn render_installed_yanked_warning(decision: &Decision) {
    if let Some(yanked) = &decision.installed_yanked {
        let reason = yanked.reason.as_deref().unwrap_or("yanked");
        println!(
            "  warning: installed version {} was yanked: {reason}",
            yanked.version
        );
    }
}

fn render_batch(
    ctx: &Ctx,
    outcome: &BatchUpdateOutcome,
    stack_hints: &[StackUpdateHint],
) -> Result<()> {
    if ctx.json {
        return render_batch_json(ctx, outcome);
    }

    if outcome.results.is_empty() {
        if let Some(filter) = outcome.target_filter {
            ctx.say(format!(
                "no direct skill install receipts found for target `{}`",
                filter.as_str()
            ));
        } else {
            ctx.say("no direct skill install receipts found");
        }
        if stack_hints.is_empty() {
            ctx.say("next: agentstack install list");
        } else if stack_hints.len() == 1 {
            let hint = &stack_hints[0];
            ctx.say("stack install receipt found");
            ctx.say(format!(
                "next: agentstack stack update {}/{} --target {}",
                hint.org,
                hint.stack,
                hint.target.as_str()
            ));
        } else {
            ctx.say("stack install receipts found; update them with:");
            for hint in stack_hints.iter().take(3) {
                ctx.say(format!(
                    "  agentstack stack update {}/{} --target {}",
                    hint.org,
                    hint.stack,
                    hint.target.as_str()
                ));
            }
            if stack_hints.len() > 3 {
                ctx.say(format!(
                    "  ... {} more; run `agentstack install list --kind stack`",
                    stack_hints.len() - 3
                ));
            }
            ctx.say("next: agentstack install list --kind stack");
        }
        return Ok(());
    }

    let name_w = outcome
        .results
        .iter()
        .map(|r| r.skill_name.len())
        .max()
        .unwrap_or(0);
    let target_w = outcome
        .results
        .iter()
        .map(|r| r.target.as_str().len())
        .max()
        .unwrap_or(0);

    for row in &outcome.results {
        let (label, detail) = match &row.status {
            BatchUpdateRowStatus::AlreadyCurrent { version } => {
                ("already current".to_string(), version.clone())
            }
            BatchUpdateRowStatus::UpdateAvailable {
                installed_version,
                latest_version,
                installed_yanked,
                changes,
            } => (
                "update available".to_string(),
                append_changes_detail(
                    append_yanked_detail(
                        format!(
                            "{} -> {}",
                            installed_version.as_deref().unwrap_or("<unknown>"),
                            latest_version
                        ),
                        installed_yanked.as_deref(),
                    ),
                    changes.as_ref(),
                ),
            ),
            BatchUpdateRowStatus::Updated {
                installed_version,
                latest_version,
                forced,
                content_drifted,
            } => {
                let label = if *forced {
                    "reinstalled".to_string()
                } else {
                    "updated".to_string()
                };
                let mut detail = if *forced {
                    latest_version.clone()
                } else {
                    format!(
                        "{} -> {}",
                        installed_version.as_deref().unwrap_or("<unknown>"),
                        latest_version
                    )
                };
                if *content_drifted {
                    detail.push_str(" (overwrote local modifications)");
                }
                (label, detail)
            }
            BatchUpdateRowStatus::Failed { reason } => ("failed".to_string(), reason.clone()),
            BatchUpdateRowStatus::Skipped { reason } => ("skipped".to_string(), reason.clone()),
        };
        ctx.say(format!(
            "{name:<name_w$}  {target:<target_w$}  {label:<17}  {detail}",
            name = row.skill_name,
            target = row.target.as_str(),
            label = label,
            detail = detail,
        ));
    }

    let summary = if outcome.check {
        format!(
            "summary: updated 0 | already-current {} | update-available {} | skipped {} | failed {}",
            outcome.already_current_count(),
            outcome.update_available_count(),
            outcome.skipped_count(),
            outcome.failed_count(),
        )
    } else {
        format!(
            "summary: updated {} | already-current {} | skipped {} | failed {}",
            outcome.updated_count(),
            outcome.already_current_count(),
            outcome.skipped_count(),
            outcome.failed_count(),
        )
    };
    ctx.say(summary);
    Ok(())
}

#[derive(Serialize)]
struct BatchJson<'a> {
    batch: bool,
    target: Option<&'static str>,
    check: bool,
    force: bool,
    results: Vec<BatchRowJson<'a>>,
    summary: BatchSummaryJson,
}

#[derive(Serialize)]
struct BatchRowJson<'a> {
    skill_name: &'a str,
    target: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_yanked: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    forced: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_drifted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changes: Option<&'a FileChangeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

#[derive(Serialize)]
struct BatchSummaryJson {
    updated: usize,
    already_current: usize,
    update_available: usize,
    skipped: usize,
    failed: usize,
}

fn render_batch_json(ctx: &Ctx, outcome: &BatchUpdateOutcome) -> Result<()> {
    let results: Vec<BatchRowJson> = outcome
        .results
        .iter()
        .map(|row| {
            let base = |status: &'static str| BatchRowJson {
                skill_name: &row.skill_name,
                target: row.target.as_str(),
                status,
                installed_version: None,
                latest_version: None,
                installed_yanked: None,
                forced: None,
                content_drifted: None,
                changes: None,
                error: None,
            };
            match &row.status {
                BatchUpdateRowStatus::AlreadyCurrent { version } => BatchRowJson {
                    installed_version: Some(version.as_str()),
                    latest_version: Some(version.as_str()),
                    ..base("already_current")
                },
                BatchUpdateRowStatus::UpdateAvailable {
                    installed_version,
                    latest_version,
                    installed_yanked,
                    changes,
                } => BatchRowJson {
                    installed_version: installed_version.as_deref(),
                    latest_version: Some(latest_version.as_str()),
                    installed_yanked: installed_yanked.as_deref(),
                    changes: changes.as_ref(),
                    ..base("update_available")
                },
                BatchUpdateRowStatus::Updated {
                    installed_version,
                    latest_version,
                    forced,
                    content_drifted,
                } => BatchRowJson {
                    installed_version: installed_version.as_deref(),
                    latest_version: Some(latest_version.as_str()),
                    forced: Some(*forced),
                    content_drifted: content_drifted.then_some(true),
                    ..base("updated")
                },
                BatchUpdateRowStatus::Failed { reason } => BatchRowJson {
                    error: Some(reason.as_str()),
                    ..base("failed")
                },
                BatchUpdateRowStatus::Skipped { reason } => BatchRowJson {
                    error: Some(reason.as_str()),
                    ..base("skipped")
                },
            }
        })
        .collect();

    let payload = BatchJson {
        batch: true,
        target: outcome.target_filter.map(|t| t.as_str()),
        check: outcome.check,
        force: outcome.force,
        results,
        summary: BatchSummaryJson {
            updated: outcome.updated_count(),
            already_current: outcome.already_current_count(),
            update_available: outcome.update_available_count(),
            skipped: outcome.skipped_count(),
            failed: outcome.failed_count(),
        },
    };
    ctx.say_always(serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn render_check(
    opts: UpdateOptions<'_>,
    decision: &Decision,
    preview: Option<&Result<FileChangeSummary>>,
) -> Result<()> {
    let (changes, changes_error) = match preview {
        Some(Ok(changes)) => (Some(changes), None),
        Some(Err(err)) => (None, Some(format!("{err:#}"))),
        None => (None, None),
    };
    // Updating a drifted install requires --force, so the suggested command
    // must carry it or the suggestion would refuse.
    let force_suffix = if decision.drift.is_drifted() {
        " --force"
    } else {
        ""
    };
    if opts.json {
        let next_command = if decision.update_available {
            Some(format!(
                "agentstack skill update {} --target {}{force_suffix}",
                opts.skill_name,
                opts.target.as_str()
            ))
        } else {
            None
        };
        let out = UpdateJson {
            skill_name: opts.skill_name,
            target: opts.target.as_str(),
            source_ref: decision.skill_ref.unversioned(),
            registry_url: opts.registry_url,
            installed_version: decision.installed_version.as_deref(),
            latest_version: &decision.latest.version,
            update_available: decision.update_available,
            installed_yanked: decision.installed_yanked.is_some(),
            installed_yank_reason: decision
                .installed_yanked
                .as_ref()
                .and_then(|yanked| yanked.reason.as_deref()),
            updated: false,
            forced: false,
            content_drifted: decision.drift.as_json_flag(),
            destination: Some(&decision.receipt.installed_path),
            receipt: None,
            cache_package: None,
            changes,
            changes_error,
            overlay: None,
            next_command,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if decision.update_available {
        println!(
            "update available for `{}`: {} -> {}",
            opts.skill_name,
            decision.installed_version.as_deref().unwrap_or("<unknown>"),
            decision.latest.version
        );
        render_installed_yanked_warning(decision);
        if let Some(changes) = changes {
            render_change_preview(changes, opts.quiet);
        } else if let Some(reason) = &changes_error
            && !opts.quiet
        {
            eprintln!("note: file change preview unavailable: {reason}");
        }
        if decision.drift.is_drifted() && !opts.quiet {
            eprintln!(
                "warning: `{}` also has local modifications; updating requires --force and overwrites them",
                opts.skill_name
            );
        }
        if !opts.quiet {
            println!(
                "next: agentstack skill update {} --target {}{force_suffix}",
                opts.skill_name,
                opts.target.as_str()
            );
        }
    } else if decision.drift.is_drifted() {
        // Up to date with the registry, but the installed files were edited
        // locally. Do not silently no-op: tell the user their changes are
        // intact and how to discard them.
        println!(
            "{}",
            drift_no_update_message(opts.skill_name, &decision.latest.version)
        );
        if !opts.quiet {
            eprintln!(
                "warning: installed files for `{}` differ from the recorded package; they were left unchanged.",
                opts.skill_name
            );
            println!("{}", drift_restore_next_line(opts.skill_name, opts.target));
        }
    } else {
        println!(
            "no update available for `{}` (current v{}).",
            opts.skill_name, decision.latest.version
        );
        if !opts.quiet {
            println!(
                "next: agentstack skill version list {}",
                decision.skill_ref.unversioned()
            );
        }
    }
    Ok(())
}

/// Most file names shown by a `--check` change preview before eliding.
const PREVIEW_FILE_CAP: usize = 20;

fn render_change_preview(changes: &FileChangeSummary, quiet: bool) {
    println!(
        "  files: {} added, {} removed, {} changed, {} unchanged",
        changes.added.len(),
        changes.removed.len(),
        changes.changed.len(),
        changes.unchanged
    );
    if quiet {
        return;
    }
    let mut shown = 0usize;
    let mut elided = 0usize;
    for (marker, paths) in [
        ("+", &changes.added),
        ("-", &changes.removed),
        ("~", &changes.changed),
    ] {
        for path in paths {
            if shown == PREVIEW_FILE_CAP {
                elided += 1;
                continue;
            }
            println!("    {marker} {path}");
            shown += 1;
        }
    }
    if elided > 0 {
        println!("    ... and {elided} more");
    }
}

fn drift_no_update_message(skill_name: &str, version: &str) -> String {
    format!(
        "no update available for `{skill_name}` (current v{version}), but the installed files have local modifications."
    )
}

fn drift_restore_next_line(skill_name: &str, target: InstallTarget) -> String {
    format!(
        "next: agentstack skill update {skill_name} --target {} --force",
        target.as_str()
    )
}

fn render_updated(
    opts: UpdateOptions<'_>,
    decision: &Decision,
    report: &super::install::RemoteInstallReport,
) -> Result<()> {
    if opts.json {
        let out = UpdateJson {
            skill_name: opts.skill_name,
            target: opts.target.as_str(),
            source_ref: decision.skill_ref.unversioned(),
            registry_url: opts.registry_url,
            installed_version: decision.installed_version.as_deref(),
            latest_version: &report.metadata.version,
            update_available: decision.update_available,
            installed_yanked: decision.installed_yanked.is_some(),
            installed_yank_reason: decision
                .installed_yanked
                .as_ref()
                .and_then(|yanked| yanked.reason.as_deref()),
            updated: true,
            forced: opts.force && !decision.update_available,
            content_drifted: decision.drift.as_json_flag(),
            destination: Some(&report.install.destination),
            receipt: report.install.receipt_path.as_deref(),
            cache_package: Some(&report.cache_entry.package_path),
            changes: None,
            changes_error: None,
            overlay: report.install.overlay.as_ref(),
            next_command: None,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    if decision.update_available {
        println!(
            "updated `{}`: {} -> {}",
            opts.skill_name,
            decision.installed_version.as_deref().unwrap_or("<unknown>"),
            report.metadata.version
        );
        render_installed_yanked_warning(decision);
    } else {
        println!(
            "reinstalled `{}` ({})",
            opts.skill_name, report.metadata.version
        );
    }
    if opts.quiet {
        return Ok(());
    }
    println!("  target:      {}", opts.target.as_str());
    println!("  destination: {}", report.install.destination.display());
    if let Some(receipt) = &report.install.receipt_path {
        println!("  receipt:     {}", receipt.display());
    }
    if let Some(overlay) = &report.install.overlay {
        println!("  applied platform overlay: {}", overlay.describe());
    }
    println!();
    println!("next:");
    println!(
        "  agentstack skill show {} --target {}",
        opts.skill_name,
        opts.target.as_str()
    );
    println!(
        "  agentstack skill version list {}",
        decision.skill_ref.unversioned()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use assert_fs::TempDir;

    use crate::receipt::{InstallVia, RECEIPT_SCHEMA_VERSION};
    use crate::registry::{MockRegistryClient, StackResolveHeader, Visibility};

    fn installed_row(
        skill_name: &str,
        target: InstallTarget,
        installed_via: Option<InstallVia>,
    ) -> InstalledRow {
        let installed_path = PathBuf::from("/tmp/agentstack-target").join(skill_name);
        InstalledRow {
            target,
            receipt_path: installed_path.join(crate::receipt::RECEIPT_FILE),
            installed_path: installed_path.clone(),
            receipt: InstallReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                skill_name: skill_name.to_string(),
                source_type: ReceiptSourceType::Registry,
                source_ref: format!("acme/{skill_name}"),
                registry_url: Some("mock://registry".to_string()),
                org: Some("acme".to_string()),
                version: Some("1".to_string()),
                hash: Some("abc123".to_string()),
                content_hash: Some("abc123".to_string()),
                target: target.as_str().to_string(),
                installed_path,
                installed_at: "2026-01-01T00:00:00Z".to_string(),
                installed_by: Some("alice@example.com".to_string()),
                installed_via,
                installed_via_stacks: Vec::new(),
            },
        }
    }

    #[test]
    fn update_all_batch_rows_skip_stack_owned_child_receipts() {
        let rows = batch_rows_from_scanned(
            vec![
                installed_row("alpha", InstallTarget::Local, None),
                installed_row(
                    "beta",
                    InstallTarget::Local,
                    Some(InstallVia {
                        kind: "stack".to_string(),
                        org: "acme".to_string(),
                        stack: "engineering-default".to_string(),
                        manifest_hash: "manifest123".to_string(),
                    }),
                ),
                installed_row("gamma", InstallTarget::Codex, None),
            ],
            Some(InstallTarget::Local),
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].skill_name, "alpha");
        assert_eq!(rows[0].target, InstallTarget::Local);
    }

    #[test]
    fn stack_update_prune_force_rejects_removed_child_path_outside_target_root() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let outside_dir = tmp.path().join("outside-skill");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("SKILL.md"), "# Outside\n").unwrap();
        let hash = PackageHash {
            algorithm: "sha256".to_string(),
            hex: "abc".to_string(),
        };
        let item = StackInstallReceiptItem {
            skill: "outside-skill".to_string(),
            version_id: "1".to_string(),
            version: "1".to_string(),
            archive_hash: hash.clone(),
            install_path: outside_dir.clone(),
            installed_receipt_path: receipt_path(&outside_dir),
        };
        let receipt = StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "engineering-default".to_string(),
            registry_url: Some("mock://registry".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: hash.clone(),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            items: vec![item.clone()],
        };
        let decision = StackDecision {
            row: InstalledStackRow {
                target: InstallTarget::Local,
                target_root: target_root.clone(),
                receipt_path: target_root
                    .join(".agentstack-stacks/acme/engineering-default/.agentstack.json"),
                receipt,
            },
            resolved: StackResolve {
                stack: StackResolveHeader {
                    org: "acme".to_string(),
                    slug: "engineering-default".to_string(),
                    name: "Engineering Default".to_string(),
                    visibility: Visibility::Org,
                    team: None,
                },
                resolved_at: "2026-01-01T00:00:00Z".to_string(),
                manifest_hash: hash,
                items: Vec::new(),
            },
            diff: StackDiff {
                added: Vec::new(),
                removed: vec![item],
                changed: Vec::new(),
                unchanged: Vec::new(),
            },
        };
        let opts = StackUpdateOptions {
            stack: "engineering-default",
            target: InstallTarget::Local,
            target_root: &target_root,
            registry_url: Some("mock://registry"),
            check: false,
            force: true,
            prune: true,
            json: false,
            quiet: true,
            installed_by: Some("octocat".to_string()),
            cache_root: None,
        };
        let mock = MockRegistryClient::new();

        let err = apply_stack_update(&mock, &opts, decision).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("outside target root"), "msg: {msg}");
        assert!(outside_dir.is_dir());
        assert!(outside_dir.join("SKILL.md").is_file());
    }

    #[test]
    fn content_drift_refusal_names_diff_and_force() {
        let err = content_drift_refusal("alpha", InstallTarget::Local);
        assert_eq!(err.code, "install_content_drifted");
        assert_eq!(
            err.message,
            "refusing to update `alpha`: installed files have local modifications; review them with `agentstack skill diff alpha --target local`, or rerun with --force to overwrite them"
        );
        assert_eq!(
            err.next_command.as_deref(),
            Some("agentstack skill diff alpha --target local")
        );
    }

    #[test]
    fn drift_no_update_copy_stays_quiet_and_actionable() {
        assert_eq!(
            drift_no_update_message("alpha", "2"),
            "no update available for `alpha` (current v2), but the installed files have local modifications."
        );
        assert_eq!(
            drift_restore_next_line("alpha", InstallTarget::Local),
            "next: agentstack skill update alpha --target local --force"
        );
    }

    fn receipt_with_registry(registry_url: Option<&str>) -> InstallReceipt {
        InstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            skill_name: "alpha".to_string(),
            source_type: ReceiptSourceType::Registry,
            source_ref: "acme/alpha".to_string(),
            registry_url: registry_url.map(str::to_string),
            org: Some("acme".to_string()),
            version: Some("1".to_string()),
            hash: Some("abc123".to_string()),
            content_hash: Some("abc123".to_string()),
            target: "local".to_string(),
            installed_path: PathBuf::from("/tmp/agentstack-target/alpha"),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }
    }

    #[test]
    fn same_registry_base_compares_normalized_registry_base() {
        assert!(same_registry_base(
            "https://registry.example.com",
            "https://registry.example.com/v1"
        ));
        assert!(same_registry_base(
            "https://Registry.Example.com",
            "https://registry.example.com"
        ));
        assert!(same_registry_base(
            "https://registry.example.com:443",
            "https://registry.example.com"
        ));
        assert!(!same_registry_base(
            "https://registry.agentstack.gg",
            "https://registry.example.com"
        ));
        assert!(!same_registry_base(
            "https://registry.example.com",
            "http://registry.example.com"
        ));
        assert!(!same_registry_base(
            "https://registry.example.com:8443",
            "https://registry.example.com"
        ));
        assert!(!same_registry_base(
            "https://registry.example.com/prod",
            "https://registry.example.com/staging"
        ));
    }

    #[test]
    fn update_refuses_cross_registry_swap_without_force() {
        let receipt = receipt_with_registry(Some("https://registry.example.com"));
        let err = ensure_same_registry(
            &receipt,
            Some("https://registry.agentstack.gg"),
            false,
            "alpha",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("different registry"), "msg: {msg}");
        assert!(msg.contains("registry.example.com"), "msg: {msg}");
        assert!(msg.contains("registry.agentstack.gg"), "msg: {msg}");
        assert!(msg.contains("--force"), "msg: {msg}");
        assert!(
            msg.contains("rerun with `--force` to replace its provenance"),
            "msg: {msg}"
        );
        assert!(!msg.contains("Re-run"), "msg: {msg}");
    }

    #[test]
    fn update_allows_cross_registry_swap_with_force() {
        let receipt = receipt_with_registry(Some("https://registry.example.com"));
        ensure_same_registry(
            &receipt,
            Some("https://registry.agentstack.gg"),
            true,
            "alpha",
        )
        .unwrap();
    }

    #[test]
    fn update_allows_same_registry_with_different_paths() {
        let receipt = receipt_with_registry(Some("https://registry.example.com/v1"));
        ensure_same_registry(
            &receipt,
            Some("https://registry.example.com"),
            false,
            "alpha",
        )
        .unwrap();
    }

    #[test]
    fn update_refuses_legacy_registry_receipt_without_url() {
        let receipt = receipt_with_registry(None);
        let err = ensure_same_registry(
            &receipt,
            Some("https://registry.agentstack.gg"),
            false,
            "alpha",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not record a registry URL"), "msg: {msg}");
        assert!(msg.contains("--force"), "msg: {msg}");
        assert!(
            msg.contains("rerun with `--force` to replace its provenance"),
            "msg: {msg}"
        );
        assert!(!msg.contains("Re-run"), "msg: {msg}");
    }

    #[test]
    fn update_allows_legacy_registry_receipt_without_url_with_force() {
        let receipt = receipt_with_registry(None);
        ensure_same_registry(
            &receipt,
            Some("https://registry.agentstack.gg"),
            true,
            "alpha",
        )
        .unwrap();
    }

    #[test]
    fn stack_update_refuses_cross_registry_swap_without_force() {
        let err = ensure_same_registry_url(
            Some("https://registry.example.com"),
            Some("https://registry.agentstack.gg"),
            false,
            "stack `acme/review-pack`",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("different registry"), "msg: {msg}");
        assert!(msg.contains("registry.example.com"), "msg: {msg}");
        assert!(msg.contains("registry.agentstack.gg"), "msg: {msg}");
        assert!(
            msg.contains("rerun with `--force` to replace its provenance"),
            "msg: {msg}"
        );
        assert!(!msg.contains("Re-run"), "msg: {msg}");
    }

    #[test]
    fn stack_update_refuses_legacy_receipt_without_url() {
        let err = ensure_same_registry_url(
            None,
            Some("https://registry.agentstack.gg"),
            false,
            "stack `acme/review-pack`",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not record a registry URL"), "msg: {msg}");
        assert!(msg.contains("--force"), "msg: {msg}");
        assert!(
            msg.contains("rerun with `--force` to replace its provenance"),
            "msg: {msg}"
        );
        assert!(!msg.contains("Re-run"), "msg: {msg}");
    }

    #[test]
    fn stack_lookup_accepts_org_qualified_refs() {
        let lookup = StackLookup::parse("acme/engineering-default").unwrap();
        assert_eq!(lookup.org.as_deref(), Some("acme"));
        assert_eq!(lookup.stack, "engineering-default");
        assert_eq!(
            lookup.label_with_org_placeholder(),
            "acme/engineering-default"
        );

        let bare = StackLookup::parse("engineering-default").unwrap();
        assert_eq!(bare.org, None);
        assert_eq!(bare.stack, "engineering-default");
        assert_eq!(
            bare.label_with_org_placeholder(),
            "<org>/engineering-default"
        );
    }
}
