use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::configured_client;
use super::doctor::{Check, Status};
use crate::config::ConfigStore;
use crate::error::CliError;
use crate::install::{
    TARGET_INSTALL_LOCK_DIR, TargetLockDiagnostics, diagnose_target_lock, format_duration,
    remove_stale_target_lock,
};
use crate::installed_scan::{
    InstalledRow, InstalledStackRow, scan_installed, scan_installed_stacks,
};
use crate::output::Ctx;
use crate::receipt::{
    InstallReceipt, InstallVia, RECEIPT_FILE, ReceiptSourceType, STACK_RECEIPT_FILE,
    StackInstallReceipt, StackLookup, ensure_stack_receipt_dir_not_symlink, read_receipt_file,
    read_stack_receipt_file, receipt_path, stack_referrers,
};
use crate::registry::{RegistryClient, VersionInfo};
use crate::skill::{check_slug, validate_skill};
use crate::skill_ref::SkillRef;
use crate::targets::{InstallTarget, TargetResolver};

/// Whether an installed skill's on-disk files still match the package hash
/// that was recorded in its install receipt.
///
/// New receipts (registry and local source alike) record `content_hash`, a
/// deterministic hash of the installed files. Older receipts only have the
/// source hash (registry archive or install tree), which is useful provenance
/// but too sensitive to historical packaging-format changes for local drift
/// checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContentDrift {
    /// Installed files re-hash to the recorded package hash.
    Matches,
    /// Installed files differ from the recorded package hash.
    Drifted { recorded: String, actual: String },
    /// Installed files no longer form a valid skill (treated as drift).
    Invalid { error: String },
    /// No recorded hash on the receipt; drift cannot be determined.
    Unknown,
}

impl ContentDrift {
    /// True when the install is known to have local modifications.
    pub(crate) fn is_drifted(&self) -> bool {
        matches!(
            self,
            ContentDrift::Drifted { .. } | ContentDrift::Invalid { .. }
        )
    }

    /// `Some(true)` drifted, `Some(false)` matches, `None` when unknown.
    pub(crate) fn as_json_flag(&self) -> Option<bool> {
        match self {
            ContentDrift::Matches => Some(false),
            ContentDrift::Drifted { .. } | ContentDrift::Invalid { .. } => Some(true),
            ContentDrift::Unknown => None,
        }
    }
}

/// Compare the installed files at `installed_path` against the content hash
/// recorded in `receipt` and classify any drift.
///
/// Older receipts without a recorded content hash are reported as
/// [`ContentDrift::Unknown`] rather than producing a false positive.
pub(crate) fn content_drift(installed_path: &Path, receipt: &InstallReceipt) -> ContentDrift {
    let Some(recorded) = receipt.content_hash.as_deref() else {
        return ContentDrift::Unknown;
    };
    match crate::install::hash_installable_tree_at(installed_path) {
        Ok(hash) => {
            let actual = crate::receipt::format_hash(&hash);
            if actual == recorded {
                ContentDrift::Matches
            } else {
                ContentDrift::Drifted {
                    recorded: recorded.to_string(),
                    actual,
                }
            }
        }
        Err(err) => ContentDrift::Invalid {
            error: format!("{err:#}"),
        },
    }
}

pub fn list(ctx: &Ctx, kind: &str, target: Option<&str>) -> Result<()> {
    match kind {
        "skill" => list_skills(ctx, target),
        "stack" => list_stacks(ctx, target),
        "all" => list_all(ctx, target),
        other => {
            bail!("unknown install receipt kind `{other}` (expected one of: skill, stack, all)")
        }
    }
}

fn skill_rows(ctx: &Ctx, target: Option<InstallTarget>) -> Result<Vec<InstalledRow>> {
    let mut rows = scan_installed(|receipt_file, e| {
        if !ctx.json && !ctx.quiet {
            eprintln!(
                "warning: skipping unreadable install receipt `{}`: {e}",
                receipt_file.display()
            );
        }
    })?;
    if let Some(target) = target {
        rows.retain(|row| row.target == target);
    }
    Ok(rows)
}

fn stack_rows(ctx: &Ctx, target: Option<InstallTarget>) -> Result<Vec<InstalledStackRow>> {
    let mut rows = scan_installed_stacks(|receipt_file, e| {
        if !ctx.json && !ctx.quiet {
            eprintln!(
                "warning: skipping unreadable stack receipt `{}`: {e}",
                receipt_file.display()
            );
        }
    })?;
    if let Some(target) = target {
        rows.retain(|row| row.target == target);
    }
    Ok(rows)
}

fn list_skills(ctx: &Ctx, target: Option<&str>) -> Result<()> {
    let target = parse_target_filter(target)?;
    let rows = skill_rows(ctx, target)?;
    if ctx.json {
        println!("{}", render_list_json(&rows, target)?);
        return Ok(());
    }

    if rows.is_empty() {
        ctx.say(installed_empty_message());
        ctx.say(format!("next: {}", installed_next_command(target)));
        note_installed_stacks(ctx, target);
        return Ok(());
    }

    let name_w = rows
        .iter()
        .map(|row| row.receipt.skill_name.len())
        .chain(std::iter::once("SKILL".len()))
        .max()
        .unwrap_or(0);
    let target_w = rows
        .iter()
        .map(|row| row.target.as_str().len())
        .chain(std::iter::once("TARGET".len()))
        .max()
        .unwrap_or(0);
    let source_w = rows
        .iter()
        .map(|row| source_label(&row.receipt).len())
        .chain(std::iter::once("SOURCE".len()))
        .max()
        .unwrap_or(0);

    println!(
        "{name:<name_w$}  {target:<target_w$}  {source:<source_w$}  {version:<10}  {hash_kind:<12}  {hash:<19}  INSTALLED",
        name = "SKILL",
        target = "TARGET",
        source = "SOURCE",
        version = "VERSION",
        hash_kind = "HASH KIND",
        hash = "HASH",
        name_w = name_w,
        target_w = target_w,
        source_w = source_w,
    );

    for row in &rows {
        let version = row.receipt.version.as_deref().unwrap_or("-");
        let hash = row
            .receipt
            .hash
            .as_deref()
            .map(short_hash)
            .unwrap_or_else(|| "-".to_string());
        let hash_kind = row
            .receipt
            .hash
            .as_ref()
            .map(|_| row.receipt.source_type.hash_kind_column())
            .unwrap_or("-");
        println!(
            "{name:<name_w$}  {target:<target_w$}  {source:<source_w$}  {version:<10}  {hash_kind:<12}  {hash:<19}  {installed_at}",
            name = row.receipt.skill_name,
            target = row.target.as_str(),
            source = source_label(&row.receipt),
            version = version,
            hash_kind = hash_kind,
            hash = hash,
            installed_at = row.receipt.installed_at,
            name_w = name_w,
            target_w = target_w,
            source_w = source_w,
        );
    }
    note_installed_stacks(ctx, target);
    Ok(())
}

/// In the default (skill) listing, point out that stacks are tracked under a
/// separate kind so installed stacks are not silently invisible here.
fn note_installed_stacks(ctx: &Ctx, target: Option<InstallTarget>) {
    if ctx.json || ctx.quiet {
        return;
    }
    let Ok(mut rows) = scan_installed_stacks(|_, _| {}) else {
        return;
    };
    if let Some(target) = target {
        rows.retain(|row| row.target == target);
    }
    if rows.is_empty() {
        return;
    }
    let mut command = String::from("agentstack install list --kind stack");
    if let Some(target) = target {
        command.push_str(&format!(" --target {}", target.as_str()));
    }
    ctx.say(format!(
        "note: {} installed stack{} not shown here — list with `{command}`",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    ));
}

fn list_stacks(ctx: &Ctx, target: Option<&str>) -> Result<()> {
    let target = parse_target_filter(target)?;
    let rows = stack_rows(ctx, target)?;
    if ctx.json {
        println!("{}", render_stack_list_json(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        ctx.say(stack_installed_empty_message());
        ctx.say("next: agentstack stack list");
        return Ok(());
    }

    let stack_w = rows
        .iter()
        .map(|row| format!("{}/{}", row.receipt.org, row.receipt.stack).len())
        .chain(std::iter::once("STACK".len()))
        .max()
        .unwrap_or(0);
    let target_w = rows
        .iter()
        .map(|row| row.target.as_str().len())
        .chain(std::iter::once("TARGET".len()))
        .max()
        .unwrap_or(0);
    let vis_w = rows
        .iter()
        .map(|row| row.receipt.visibility.as_str().len())
        .chain(std::iter::once("VISIBILITY".len()))
        .max()
        .unwrap_or(0);

    println!(
        "{label:<stack_w$}  {target:<target_w$}  {visibility:<vis_w$}  {items:<5}  HASH  INSTALLED",
        label = "STACK",
        target = "TARGET",
        visibility = "VISIBILITY",
        items = "ITEMS",
        stack_w = stack_w,
        target_w = target_w,
        vis_w = vis_w,
    );

    for row in &rows {
        let label = format!("{}/{}", row.receipt.org, row.receipt.stack);
        let hash = row.receipt.manifest_hash.short();
        println!(
            "{label:<stack_w$}  {target:<target_w$}  {visibility:<vis_w$}  {items:<5}  sha256:{hash}  {installed_at}",
            label = label,
            target = row.target.as_str(),
            visibility = row.receipt.visibility.as_str(),
            items = row.receipt.items.len(),
            hash = hash,
            installed_at = row.receipt.installed_at,
            stack_w = stack_w,
            target_w = target_w,
            vis_w = vis_w,
        );
    }
    Ok(())
}

fn list_all(ctx: &Ctx, target: Option<&str>) -> Result<()> {
    let target = parse_target_filter(target)?;
    let skill_rows = skill_rows(ctx, target)?;
    let stack_rows = stack_rows(ctx, target)?;
    if ctx.json {
        println!("{}", render_all_list_json(&skill_rows, &stack_rows)?);
        return Ok(());
    }

    if skill_rows.is_empty() && stack_rows.is_empty() {
        ctx.say(all_installed_empty_message());
        ctx.say(format!("next: {}", installed_next_command(target)));
        return Ok(());
    }

    ctx.say("skills:");
    list_skills(ctx, target.map(|target| target.as_str()))?;
    ctx.say("");
    ctx.say("stacks:");
    list_stacks(ctx, target.map(|target| target.as_str()))?;
    Ok(())
}

enum InstalledSubject<'a> {
    Skill(&'a str),
    Stack(&'a str),
}

pub fn inspect(
    ctx: &Ctx,
    subject: &str,
    subject_name: Option<&str>,
    target_name: &str,
) -> Result<()> {
    let subject = match (subject, subject_name) {
        ("skill", Some(name)) => InstalledSubject::Skill(name),
        ("stack", Some(name)) => InstalledSubject::Stack(name),
        ("skill", None) => {
            bail!("`agentstack skill show <skill> --target <target>` requires a skill name")
        }
        ("stack", None) => {
            bail!("`agentstack stack show <stack> --target <target>` requires a stack name")
        }
        (_, Some(_)) => {
            bail!("unknown install receipt inspect kind `{subject}` (expected `skill` or `stack`)")
        }
        (name, None) => InstalledSubject::Skill(name),
    };
    match subject {
        InstalledSubject::Skill(skill_name) => inspect_skill(ctx, skill_name, target_name),
        InstalledSubject::Stack(stack) => inspect_stack(ctx, stack, target_name),
    }
}

fn inspect_skill(ctx: &Ctx, skill_name: &str, target_name: &str) -> Result<()> {
    check_slug(skill_name)
        .map_err(|reason| anyhow::anyhow!("invalid skill name `{skill_name}`: {reason}"))?;
    let resolved = resolve_target(target_name)?;
    let target = resolved.target;
    let installed_path = resolved.path.join(skill_name);
    let receipt_file = receipt_path(&installed_path);
    if !receipt_file.is_file() {
        return Err(skill_receipt_missing(skill_name, target, "show").into());
    }
    let receipt = read_receipt_file(&receipt_file).with_context(|| {
        format!(
            "failed to read install receipt for `{skill_name}` in target `{}`",
            target.as_str()
        )
    })?;
    let validation = validate_skill(&installed_path);
    let drift = content_drift(&installed_path, &receipt);

    if ctx.json {
        let payload = InstalledInspectJson {
            receipt: &receipt,
            receipt_path: &receipt_file,
            validation: InstalledValidationJson {
                ok: validation.is_ok(),
                errors: &validation.errors,
            },
            hash_kind: receipt
                .hash
                .as_ref()
                .map(|_| receipt.source_type.hash_kind()),
            content_drifted: drift.as_json_flag(),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!("installed skill {}", receipt.skill_name));
    ctx.say(format!("path: {}", receipt.installed_path.display()));
    ctx.say(format!("target: {}", receipt.target));
    match receipt.source_type.as_str() {
        "registry" => {
            ctx.say(format!("source: registry {}", receipt.source_ref));
            if let Some(url) = &receipt.registry_url {
                ctx.say(format!("registry: {url}"));
            }
            if let Some(version) = &receipt.version {
                ctx.say(format!("version: {version}"));
            }
        }
        _ => ctx.say(format!("source: local {}", receipt.source_ref)),
    }
    if let Some(hash) = &receipt.hash {
        ctx.say(format!("{}: {hash}", receipt.source_type.hash_label()));
    }
    ctx.say(format!("installed at: {}", receipt.installed_at));
    if let Some(user) = &receipt.installed_by {
        ctx.say(format!("installed by: {user}"));
    }
    let stacks = stack_referrers(&receipt);
    if !stacks.is_empty() {
        ctx.say("required by:");
        for via in &stacks {
            ctx.say(format!("  - stack {}/{}", via.org, via.stack));
        }
    }
    ctx.say(format!("receipt: {}", receipt_file.display()));
    if validation.is_ok() {
        ctx.say("validation: ok");
    } else {
        ctx.say("validation: failed");
        for error in &validation.errors {
            ctx.say(format!("  - {error}"));
        }
    }
    render_content_drift_line(ctx, &drift, &receipt);
    ctx.say("");
    if let Some(via) = stacks.first() {
        ctx.say(format!(
            "next: agentstack stack update {}/{} --target {} --check",
            via.org, via.stack, receipt.target
        ));
    } else if receipt.source_type == ReceiptSourceType::Registry {
        ctx.say(format!(
            "next: agentstack skill update {} --target {} --check",
            receipt.skill_name, receipt.target
        ));
    } else {
        ctx.say("registry update checks require a registry install receipt.");
    }
    Ok(())
}

pub fn why(ctx: &Ctx, skill_name: &str, target_name: &str) -> Result<()> {
    check_slug(skill_name)
        .map_err(|reason| anyhow::anyhow!("invalid skill name `{skill_name}`: {reason}"))?;
    let resolved = resolve_target(target_name)?;
    let target = resolved.target;
    let installed_path = resolved.path.join(skill_name);
    let receipt_file = receipt_path(&installed_path);
    let receipt = read_install_why_receipt(&receipt_file, skill_name, target)?;
    let stacks = stack_referrers(&receipt);
    let provenance = provenance(&stacks);
    let direct_install = provenance == Provenance::Direct;
    let freshness = registry_freshness(&receipt, ctx.verbose);
    let safe_to_remove = stacks.is_empty();

    if ctx.json {
        let payload = InstalledWhyJson {
            skill: &receipt.skill_name,
            target: target.as_str(),
            source_type: receipt.source_type.as_str(),
            source_ref: &receipt.source_ref,
            installed_version: receipt.version.as_deref(),
            current_version_known: freshness.current_version.is_some(),
            current_version: freshness.current_version.as_deref(),
            current_registry_version: freshness.current_version.as_deref(),
            update_available: freshness.update_available,
            registry_check_status: freshness.status.as_str(),
            registry_current: freshness.current.as_ref(),
            registry_check_error: freshness.error.as_deref(),
            provenance: provenance.as_str(),
            required_by_stacks: stacks.iter().map(stack_label).collect(),
            installed_by: InstalledByJson {
                direct: direct_install,
                stacks: stacks.iter().map(stack_label).collect(),
            },
            direct_remove_safe: safe_to_remove,
            safe_to_remove,
            reason: remove_reason(&stacks),
            next_command: install_why_next_command(&receipt.skill_name, target, safe_to_remove),
            receipt_path: &receipt_file,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!("skill: {}", receipt_display_name(&receipt)));
    ctx.say(format!("target: {}", target.as_str()));
    ctx.say(format!(
        "installed version: {}",
        version_label(receipt.version.as_deref())
    ));
    ctx.say(format!(
        "current registry version: {}",
        freshness_label(&freshness)
    ));
    render_lifecycle_warning(ctx, &freshness);
    ctx.say("installed by:");
    if stacks.is_empty() {
        ctx.say("  - direct install");
    } else {
        for via in &stacks {
            ctx.say(format!("  - stack {}/{}", via.org, via.stack));
        }
    }
    ctx.say(format!(
        "direct install: {}",
        if direct_install { "yes" } else { "no" }
    ));
    ctx.say(format!(
        "safe to remove: {}",
        if safe_to_remove {
            "yes".to_string()
        } else {
            format!(
                "no, still required by {} stack{}",
                stacks.len(),
                plural(stacks.len())
            )
        }
    ));
    ctx.say(format!(
        "next: {}",
        install_why_next_command(&receipt.skill_name, target, safe_to_remove)
    ));
    Ok(())
}

fn install_why_next_command(
    skill_name: &str,
    target: InstallTarget,
    safe_to_remove: bool,
) -> String {
    if safe_to_remove {
        format!(
            "agentstack skill uninstall {} --target {} --dry-run",
            skill_name,
            target.as_str()
        )
    } else {
        format!(
            "agentstack skill show {} --target {}",
            skill_name,
            target.as_str()
        )
    }
}

fn skill_receipt_missing(skill_name: &str, target: InstallTarget, action: &str) -> CliError {
    CliError::new(
        "install_receipt_missing",
        format!("no install receipt for `{skill_name}` in target `{target}`"),
    )
    .resource(skill_name)
    .action(action)
    .next_command(format!(
        "agentstack install list --target {}",
        target.as_str()
    ))
}

fn read_install_why_receipt(
    receipt_file: &Path,
    skill_name: &str,
    target: InstallTarget,
) -> Result<InstallReceipt> {
    if !receipt_file.is_file() {
        return Err(skill_receipt_missing(skill_name, target, "install_why").into());
    }
    read_receipt_file(receipt_file).map_err(|err| {
        CliError::new(
            "install_receipt_invalid",
            format!("invalid install receipt for `{skill_name}` in target `{target}`: {err:#}"),
        )
        .resource(receipt_file.display().to_string())
        .action("install_why")
        .next_command(format!(
            "agentstack skill uninstall {skill_name} --target {} --force",
            target.as_str()
        ))
        .into()
    })
}

fn inspect_stack(ctx: &Ctx, stack: &str, target_name: &str) -> Result<()> {
    let lookup = StackLookup::parse(stack)?;
    let resolved = resolve_target(target_name)?;
    let target = resolved.target;
    let row = find_stack_row(&resolved.path, target, &lookup)?;

    if ctx.json {
        let payload = InstalledStackInspectJson {
            receipt: &row.receipt,
            receipt_path: &row.receipt_path,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!(
        "installed stack {}/{}",
        row.receipt.org, row.receipt.stack
    ));
    ctx.say(format!("target: {}", row.receipt.target));
    ctx.say(format!("visibility: {}", row.receipt.visibility));
    if let Some(team) = &row.receipt.team {
        ctx.say(format!("team: {team}"));
    }
    ctx.say(format!(
        "manifest hash: {}",
        crate::receipt::format_hash(&row.receipt.manifest_hash)
    ));
    ctx.say(format!("resolved at: {}", row.receipt.resolved_at));
    ctx.say(format!("installed at: {}", row.receipt.installed_at));
    if let Some(user) = &row.receipt.installed_by {
        ctx.say(format!("installed by: {user}"));
    }
    ctx.say(format!("receipt: {}", row.receipt_path.display()));
    ctx.say("items:");
    for item in &row.receipt.items {
        ctx.say(format!(
            "  - {}@{} -> {}",
            item.skill,
            item.version,
            item.install_path.display()
        ));
    }
    ctx.say("");
    ctx.say(format!(
        "next: agentstack stack update {}/{} --target {} --check",
        row.receipt.org, row.receipt.stack, row.receipt.target
    ));
    Ok(())
}

fn find_stack_row(
    target_root: &Path,
    target: InstallTarget,
    lookup: &StackLookup,
) -> Result<InstalledStackRow> {
    if target_root.exists() && !target_root.is_dir() {
        bail!(
            "target `{}` path `{}` exists but is not a directory",
            target.as_str(),
            target_root.display()
        );
    }

    let stacks_root = target_root.join(".agentstack-stacks");
    let mut rows = Vec::new();
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    if stacks_root.exists() {
        if !stacks_root.is_dir() {
            bail!(
                "stack receipt root `{}` exists but is not a directory",
                stacks_root.display()
            );
        }

        if let Some(org) = lookup.org.as_deref() {
            let org_path = stacks_root.join(org);
            ensure_stack_receipt_dir_not_symlink(&org_path)?;
            if org_path.is_dir() {
                let stack_path = org_path.join(&lookup.stack);
                ensure_stack_receipt_dir_not_symlink(&stack_path)?;
                if stack_path.is_dir() {
                    let candidate = stack_path.join(STACK_RECEIPT_FILE);
                    if let Some(row) =
                        read_matching_stack_row(&candidate, target_root, target, lookup)
                    {
                        rows.push(row);
                    }
                }
            }
        } else {
            for org_entry in fs::read_dir(&stacks_root)
                .with_context(|| format!("failed to read `{}`", stacks_root.display()))?
            {
                let org_entry = org_entry.with_context(|| {
                    format!("failed to read entry in `{}`", stacks_root.display())
                })?;
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
                if let Some(row) = read_matching_stack_row(&candidate, target_root, target, lookup)
                {
                    rows.push(row);
                }
            }
        }
    }

    match rows.len() {
        0 => Err(CliError::new(
            "install_receipt_missing",
            format!(
                "no stack install receipt for `{}` in target `{}`",
                lookup.label(),
                target.as_str()
            ),
        )
        .resource(lookup.label())
        .action("show")
        .next_command(format!(
            "agentstack stack install {} --target {}",
            lookup.label(),
            target.as_str()
        ))
        .into()),
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => bail!(
            "multiple stack install receipts named `{}` found in target `{}`; use `org/stack` or remove the duplicate receipt",
            lookup.stack,
            target.as_str()
        ),
    }
}

fn read_matching_stack_row(
    receipt_path: &Path,
    target_root: &Path,
    target: InstallTarget,
    lookup: &StackLookup,
) -> Option<InstalledStackRow> {
    if !receipt_path.is_file() {
        return None;
    }
    let Ok(receipt) = read_stack_receipt_file(receipt_path) else {
        return None;
    };
    if receipt.stack != lookup.stack || lookup.org.as_deref().is_some_and(|org| receipt.org != org)
    {
        return None;
    }

    Some(InstalledStackRow {
        target,
        target_root: target_root.to_path_buf(),
        receipt_path: receipt_path.to_path_buf(),
        receipt,
    })
}

pub fn doctor(ctx: &Ctx, target_name: &str) -> Result<()> {
    let resolved = resolve_target(target_name)?;
    let mut report = target_doctor_report(resolved.target, &resolved.path)?;
    report.lifecycle = doctor_lifecycle_checks(resolved.target.as_str(), &resolved.path);
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    ctx.say(format!("install target {}", report.target));
    ctx.say(format!("  path: {}", report.target_root.display()));
    ctx.say(format!("  exists: {}", yes_no(report.target_exists)));
    ctx.say(format!("  lock: {}", yes_no(report.lock.exists)));
    if report.lock.exists {
        ctx.say(format!("  lock path: {}", report.lock.lock_path.display()));
        ctx.say(format!(
            "  lock age: {}",
            report
                .lock
                .age_seconds
                .map(|age| format_duration(Duration::from_secs(age)))
                .unwrap_or_else(|| "unknown".to_string())
        ));
        ctx.say(format!("  lock stale: {}", yes_no(report.lock.stale)));
        if let Some(metadata) = &report.lock.metadata {
            ctx.say(format!("  lock pid: {}", metadata.pid));
            if let Some(hostname) = &metadata.hostname {
                ctx.say(format!("  lock hostname: {hostname}"));
            }
            if let Some(kind) = &metadata.command_kind {
                ctx.say(format!("  lock command: {kind}"));
            }
        }
        if let Some(error) = &report.lock.metadata_error {
            ctx.say(format!("  lock metadata error: {error}"));
        }
    }
    ctx.say(format!("  staging dirs: {}", report.staging_dirs.len()));
    ctx.say(format!(
        "  receipts: {} parseable, {} unreadable",
        report.receipts_parseable, report.receipts_unreadable
    ));
    if report.recorded_package_matches.is_empty()
        && report.drifted.is_empty()
        && report.unknown.is_empty()
    {
        ctx.say("  content: no direct skill installs found");
    } else if report.drifted.is_empty() && report.unknown.is_empty() {
        ctx.say("  content: all installs match recorded packages");
    } else {
        ctx.say(format!(
            "  content: {} matched recorded packages, {} drifted, {} unverified",
            report.recorded_package_matches.len(),
            report.drifted.len(),
            report.unknown.len()
        ));
        for drift in &report.drifted {
            ctx.say(format!(
                "  [warn]  {} content modified — {} (run `{}` to restore)",
                drift.skill, drift.detail, drift.restore_command
            ));
        }
        for unknown in &report.unknown {
            ctx.say(format!(
                "  [info]  {} content unverified — {} ({})",
                unknown.skill, unknown.reason, unknown.source
            ));
        }
    }
    for check in &report.lifecycle {
        let fix = check
            .fix_command
            .as_deref()
            .map(|fix| format!(" (run `{fix}`)"))
            .unwrap_or_default();
        match check.status {
            Status::Ok => ctx.say(format!("  note: {}{fix}", check.detail)),
            Status::Warn => ctx.say(format!("  [warn]  {}{fix}", check.detail)),
            Status::Fail => ctx.say(format!("  [fail]  {}{fix}", check.detail)),
        }
    }
    ctx.say("mutation: none");
    if report.lock.exists {
        if report.lock.stale {
            ctx.say(format!(
                "next: agentstack install unlock --target {}",
                report.target
            ));
        } else {
            ctx.say(format!(
                "next: wait for the active install/update, or run `agentstack install unlock --target {} --force` after confirming no AgentStack process is active",
                report.target
            ));
        }
    }
    Ok(())
}

pub fn unlock(ctx: &Ctx, target_name: &str, force: bool) -> Result<()> {
    let resolved = resolve_target(target_name)?;
    let before = remove_stale_target_lock(&resolved.path, force)?;
    let removed = before.exists;

    if ctx.json {
        let payload = InstalledUnlockJson {
            target: resolved.target.as_str(),
            target_root: &resolved.path,
            removed,
            force,
            lock: before,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if removed {
        if force {
            ctx.say(format!(
                "removed install lock for target {} with --force",
                resolved.target
            ));
            ctx.say("warning: --force bypassed the stale-lock age check");
        } else {
            ctx.say(format!(
                "removed stale install lock for target {}",
                resolved.target
            ));
        }
    } else {
        ctx.say(format!(
            "no install lock found for target {}",
            resolved.target
        ));
    }
    Ok(())
}

fn resolve_target(target_name: &str) -> Result<crate::targets::ResolvedTarget> {
    let target = InstallTarget::parse(target_name)?;
    let store = ConfigStore::load().context("failed to load config")?;
    let resolver = TargetResolver::new(&store);
    resolver.resolve(target)
}

fn parse_target_filter(target: Option<&str>) -> Result<Option<InstallTarget>> {
    target.map(InstallTarget::parse).transpose()
}

fn target_doctor_report(target: InstallTarget, target_root: &Path) -> Result<InstalledDoctorJson> {
    let target_exists = target_root.exists();
    let target_is_dir = target_root.is_dir();
    let lock = diagnose_target_lock(target_root);
    let staging_dirs = if target_is_dir {
        agentstack_staging_dirs(target_root)?
    } else {
        Vec::new()
    };
    let (receipts_parseable, receipts_unreadable) = if target_is_dir {
        receipt_counts(target_root)?
    } else {
        (0, 0)
    };
    let (recorded_package_matches, drifted, unknown) = if target_is_dir {
        classified_content_installs(target_root)?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    Ok(InstalledDoctorJson {
        target: target.as_str(),
        target_root: target_root.to_path_buf(),
        target_exists,
        target_is_dir,
        lock,
        staging_dirs,
        receipts_parseable,
        receipts_unreadable,
        recorded_package_matches,
        drifted,
        unknown,
        lifecycle: Vec::new(),
    })
}

/// Stable check codes for the registry lifecycle section of `install doctor`.
const CHECK_INSTALLED_VERSION_YANKED: &str = "installed_version_yanked";
const CHECK_INSTALLED_VERSION_DEPRECATED: &str = "installed_version_deprecated";
const CHECK_INSTALLED_VERSION_OUTDATED: &str = "installed_version_outdated";
const CHECK_REGISTRY_LIFECYCLE_SKIPPED: &str = "registry_lifecycle_skipped";

/// Run the registry lifecycle checks for `install doctor`.
///
/// Network state must never fail the doctor run: when no client can be
/// configured (no registry, no token) this degrades to a single skip note.
fn doctor_lifecycle_checks(target: &str, target_root: &Path) -> Vec<Check> {
    let receipts = direct_registry_receipts(target_root);
    if receipts.is_empty() {
        return Vec::new();
    }
    let configured = match configured_client() {
        Ok(configured) => configured,
        Err(err) => return vec![lifecycle_skipped_check(&format!("{err:#}"))],
    };
    // Only consult the registry each receipt was installed from: querying a
    // different active registry would leak installed org/skill names to it
    // and return lifecycle state for the wrong server.
    let (matching, foreign): (Vec<_>, Vec<_>) = receipts.into_iter().partition(|receipt| {
        receipt
            .registry_url
            .as_deref()
            .is_some_and(|url| super::update::same_registry_base(url, &configured.url))
    });
    let mut checks = if matching.is_empty() {
        Vec::new()
    } else {
        registry_lifecycle_checks(target, &matching, &configured.client)
    };
    if !foreign.is_empty() {
        checks.push(lifecycle_skipped_check(&format!(
            "{} install{} recorded against a different registry than the active one",
            foreign.len(),
            if foreign.len() == 1 { "" } else { "s" }
        )));
    }
    checks
}

/// Best-effort scan of direct skill install receipts that point at a registry
/// source (org + version recorded), sorted by skill name.
fn direct_registry_receipts(target_root: &Path) -> Vec<InstallReceipt> {
    let mut receipts = Vec::new();
    let Ok(entries) = fs::read_dir(target_root) else {
        return receipts;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir()
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == TARGET_INSTALL_LOCK_DIR)
        {
            continue;
        }
        let receipt_file = path.join(RECEIPT_FILE);
        if !receipt_file.is_file() {
            continue;
        }
        let Ok(receipt) = read_receipt_file(&receipt_file) else {
            continue;
        };
        if receipt.source_type == ReceiptSourceType::Registry
            && receipt.org.is_some()
            && receipt.version.is_some()
        {
            receipts.push(receipt);
        }
    }
    receipts.sort_by(|a, b| a.skill_name.cmp(&b.skill_name));
    receipts
}

/// Compare each registry-sourced install against the registry's version
/// lifecycle metadata. One `list_versions` call per skill; results are cached
/// so multiple receipts for the same skill do not refetch. A registry error
/// for one skill (404, permission) never aborts the pass — the remaining
/// receipts are still checked and the failures roll up into a single note.
fn registry_lifecycle_checks(
    target: &str,
    receipts: &[InstallReceipt],
    client: &dyn RegistryClient,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut cache: BTreeMap<(String, String), Vec<VersionInfo>> = BTreeMap::new();
    let mut outdated = 0usize;
    let mut uncheckable: Vec<String> = Vec::new();
    for receipt in receipts {
        let (Some(org), Some(installed_version)) =
            (receipt.org.as_deref(), receipt.version.as_deref())
        else {
            continue;
        };
        let key = (org.to_string(), receipt.skill_name.clone());
        let versions = match cache.entry(key) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let Ok(skill_ref) = SkillRef::new(org, &receipt.skill_name) else {
                    continue;
                };
                match client.list_versions(&skill_ref) {
                    Ok(versions) => entry.insert(versions),
                    Err(err) => {
                        // A failure for one skill must not hide yanked
                        // versions on the receipts we can still check.
                        uncheckable.push(format!("{}: {err:#}", receipt.skill_name));
                        entry.insert(Vec::new())
                    }
                }
            }
        };
        let Some(installed) = versions
            .iter()
            .find(|version| version.version == installed_version)
        else {
            continue;
        };
        let current = versions
            .iter()
            .find(|version| version.current.unwrap_or(false));
        let updatable = current.is_some_and(|current| current.version != installed.version);
        // Stack-owned children reject direct `skill update`/`uninstall`; the
        // managed path is updating the owning stack.
        let fix = if let Some(via) = receipt.installed_via_stacks.first() {
            format!(
                "agentstack stack update {}/{} --target {target}",
                via.org, via.stack
            )
        } else if updatable {
            format!(
                "agentstack skill update {} --target {target}",
                receipt.skill_name
            )
        } else {
            format!(
                "agentstack skill uninstall {} --target {target}",
                receipt.skill_name
            )
        };
        if installed.yanked_at.is_some() {
            let reason = sanitize_for_terminal(
                installed
                    .yank_reason
                    .as_deref()
                    .unwrap_or("no reason recorded"),
            );
            checks.push(Check {
                code: CHECK_INSTALLED_VERSION_YANKED.to_string(),
                name: receipt.skill_name.clone(),
                status: Status::Fail,
                detail: format!(
                    "installed version {org}/{}@{installed_version} was yanked — {reason}",
                    receipt.skill_name
                ),
                fix_command: Some(fix),
            });
        } else if installed.deprecated_at.is_some() {
            let reason = sanitize_for_terminal(
                installed
                    .deprecation_reason
                    .as_deref()
                    .unwrap_or("no reason recorded"),
            );
            checks.push(Check {
                code: CHECK_INSTALLED_VERSION_DEPRECATED.to_string(),
                name: receipt.skill_name.clone(),
                status: Status::Warn,
                detail: format!(
                    "installed version {org}/{}@{installed_version} is deprecated — {reason}",
                    receipt.skill_name
                ),
                fix_command: updatable.then_some(fix),
            });
        } else if updatable {
            outdated += 1;
        }
    }
    if !uncheckable.is_empty() {
        checks.push(lifecycle_skipped_check(&format!(
            "{} skill{} could not be checked — {}",
            uncheckable.len(),
            if uncheckable.len() == 1 { "" } else { "s" },
            uncheckable.join("; ")
        )));
    }
    if outdated > 0 {
        checks.push(Check {
            code: CHECK_INSTALLED_VERSION_OUTDATED.to_string(),
            name: "registry lifecycle".to_string(),
            status: Status::Ok,
            detail: if outdated == 1 {
                "1 install has a newer approved version".to_string()
            } else {
                format!("{outdated} installs have newer approved versions")
            },
            fix_command: Some("agentstack install update --all".to_string()),
        });
    }
    checks
}

fn lifecycle_skipped_check(reason: &str) -> Check {
    Check {
        code: CHECK_REGISTRY_LIFECYCLE_SKIPPED.to_string(),
        name: "registry lifecycle".to_string(),
        status: Status::Ok,
        detail: format!(
            "registry lifecycle checks skipped — {}",
            sanitize_for_terminal(reason)
        ),
        fix_command: None,
    }
}

/// Registry-controlled strings (yank/deprecation reasons, error bodies) are
/// rendered to the terminal; strip control characters so a compromised
/// registry cannot inject ANSI escapes.
fn sanitize_for_terminal(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Skill install receipt files in the immediate children of `target_root`,
/// skipping the lock directory.
fn direct_receipt_files(target_root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(target_root)
        .with_context(|| format!("failed to read `{}`", target_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read entry in `{}`", target_root.display()))?;
        let path = entry.path();
        if !path.is_dir()
            || path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == TARGET_INSTALL_LOCK_DIR)
        {
            continue;
        }
        let receipt_file = path.join(RECEIPT_FILE);
        if receipt_file.is_file() {
            files.push(receipt_file);
        }
    }
    Ok(files)
}

/// Scan direct skill install receipts under `target_root` and classify whether
/// installed files match, drift from, or cannot be checked against the
/// recorded content hash.
fn classified_content_installs(
    target_root: &Path,
) -> Result<(
    Vec<MatchedInstallJson>,
    Vec<DriftedInstallJson>,
    Vec<UnknownInstallJson>,
)> {
    let mut recorded_package_matches = Vec::new();
    let mut drifted = Vec::new();
    let mut unknown = Vec::new();
    for receipt_file in direct_receipt_files(target_root)? {
        let Some(path) = receipt_file.parent() else {
            continue;
        };
        let Ok(receipt) = read_receipt_file(&receipt_file) else {
            continue;
        };
        match content_drift(path, &receipt) {
            ContentDrift::Matches => recorded_package_matches.push(MatchedInstallJson {
                skill: receipt.skill_name,
            }),
            ContentDrift::Drifted { recorded, actual } => drifted.push(DriftedInstallJson {
                restore_command: drift_restore_command(&receipt),
                skill: receipt.skill_name,
                detail: format!("recorded {recorded}, actual {actual}"),
            }),
            ContentDrift::Invalid { error } => drifted.push(DriftedInstallJson {
                restore_command: drift_restore_command(&receipt),
                skill: receipt.skill_name,
                detail: format!("installed files are no longer a valid skill ({error})"),
            }),
            ContentDrift::Unknown => {
                let source = content_unknown_source(&receipt);
                unknown.push(UnknownInstallJson {
                    skill: receipt.skill_name,
                    source,
                    reason: "no recorded content hash".to_string(),
                });
            }
        }
    }
    recorded_package_matches.sort_by(|a, b| a.skill.cmp(&b.skill));
    drifted.sort_by(|a, b| a.skill.cmp(&b.skill));
    unknown.sort_by(|a, b| a.skill.cmp(&b.skill));
    Ok((recorded_package_matches, drifted, unknown))
}

fn content_unknown_source(receipt: &InstallReceipt) -> String {
    match &receipt.source_type {
        ReceiptSourceType::Registry => "registry receipt without content hash".to_string(),
        _ => "local receipt without content hash".to_string(),
    }
}

fn agentstack_staging_dirs(target_root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in fs::read_dir(target_root)
        .with_context(|| format!("failed to read `{}`", target_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read entry in `{}`", target_root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".agentstack-install-")
            || name.starts_with(".agentstack-stack-install-")
            || name.starts_with(".agentstack-install-backup-")
            || name.starts_with(".agentstack-stack-backup-")
        {
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn receipt_counts(target_root: &Path) -> Result<(usize, usize)> {
    let stacks_root = target_root.join(".agentstack-stacks");
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;

    let mut parseable = 0usize;
    let mut unreadable = 0usize;
    for receipt in direct_receipt_files(target_root)? {
        match read_receipt_file(&receipt) {
            Ok(_) => parseable += 1,
            Err(_) => unreadable += 1,
        }
    }
    if stacks_root.is_dir() {
        count_stack_receipts(&stacks_root, &mut parseable, &mut unreadable)?;
    }
    Ok((parseable, unreadable))
}

fn count_stack_receipts(root: &Path, parseable: &mut usize, unreadable: &mut usize) -> Result<()> {
    ensure_stack_receipt_dir_not_symlink(root)?;
    for entry in
        fs::read_dir(root).with_context(|| format!("failed to read `{}`", root.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in `{}`", root.display()))?;
        let path = entry.path();
        ensure_stack_receipt_dir_not_symlink(&path)?;
        if !path.is_dir() {
            continue;
        }
        let receipt = path.join(STACK_RECEIPT_FILE);
        if receipt.is_file() {
            match read_stack_receipt_file(&receipt) {
                Ok(_) => *parseable += 1,
                Err(_) => *unreadable += 1,
            }
            continue;
        }
        count_stack_receipts(&path, parseable, unreadable)?;
    }
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

struct RegistryFreshness {
    status: RegistryCheckStatus,
    current_version: Option<String>,
    update_available: Option<bool>,
    current: Option<VersionInfo>,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistryCheckStatus {
    LocalInstall,
    Ok,
    Unavailable,
    NotFound,
    Unauthorized,
    InvalidReceipt,
    Unknown,
}

impl RegistryCheckStatus {
    const fn as_str(self) -> &'static str {
        match self {
            RegistryCheckStatus::LocalInstall => "local_install",
            RegistryCheckStatus::Ok => "ok",
            RegistryCheckStatus::Unavailable => "unavailable",
            RegistryCheckStatus::NotFound => "not_found",
            RegistryCheckStatus::Unauthorized => "unauthorized",
            RegistryCheckStatus::InvalidReceipt => "invalid_receipt",
            RegistryCheckStatus::Unknown => "unknown",
        }
    }
}

fn registry_freshness(receipt: &InstallReceipt, verbose: bool) -> RegistryFreshness {
    if receipt.source_type != ReceiptSourceType::Registry {
        return RegistryFreshness {
            status: RegistryCheckStatus::LocalInstall,
            current_version: None,
            update_available: None,
            current: None,
            error: None,
        };
    }
    match registry_freshness_result(receipt) {
        Ok(freshness) => freshness,
        Err(err) => {
            if verbose {
                eprintln!("[verbose] install why registry freshness check failed: {err:#}");
            }
            RegistryFreshness {
                status: classify_registry_check_error(&err),
                current_version: None,
                update_available: None,
                current: None,
                error: Some(err.to_string()),
            }
        }
    }
}

fn registry_freshness_result(receipt: &InstallReceipt) -> Result<RegistryFreshness> {
    let Some(org) = receipt.org.as_deref() else {
        bail!("registry install receipt is missing org");
    };
    let skill_ref = SkillRef::new(org, &receipt.skill_name)?;
    let configured = configured_client()?;
    let versions = configured
        .client
        .list_versions(&skill_ref)
        .with_context(|| format!("versions request to {} failed", configured.url))?;
    let current = current_version(&versions)?;
    let update_available = receipt.version.as_deref() != Some(current.version.as_str())
        || receipt.hash.as_deref() != Some(crate::receipt::format_hash(&current.hash).as_str());
    Ok(RegistryFreshness {
        status: RegistryCheckStatus::Ok,
        current_version: Some(current.version.clone()),
        update_available: Some(update_available),
        current: Some(current.clone()),
        error: None,
    })
}

fn classify_registry_check_error(err: &anyhow::Error) -> RegistryCheckStatus {
    for cause in err.chain() {
        if let Some(cli_error) = cause.downcast_ref::<CliError>() {
            match (cli_error.code.as_str(), cli_error.http_status) {
                ("unauthenticated", _) => return RegistryCheckStatus::Unauthorized,
                ("skill_not_found", _) => return RegistryCheckStatus::NotFound,
                (_, Some(401 | 403)) => return RegistryCheckStatus::Unauthorized,
                (_, Some(404)) => return RegistryCheckStatus::NotFound,
                _ => {}
            }
        }
        let text = cause.to_string();
        if text.contains("registry request failed") {
            return RegistryCheckStatus::Unavailable;
        }
        if text.contains("registry install receipt is missing org") {
            return RegistryCheckStatus::InvalidReceipt;
        }
    }
    RegistryCheckStatus::Unknown
}

fn current_version(versions: &[VersionInfo]) -> Result<&VersionInfo> {
    versions
        .iter()
        .find(|version| version.current.unwrap_or(false))
        .context("skill has no approved/current registry version")
}

fn receipt_display_name(receipt: &InstallReceipt) -> String {
    receipt
        .org
        .as_ref()
        .map(|org| format!("{org}/{}", receipt.skill_name))
        .unwrap_or_else(|| receipt.skill_name.clone())
}

fn version_label(version: Option<&str>) -> String {
    version
        .map(|version| format!("v{version}"))
        .unwrap_or_else(|| "-".to_string())
}

fn freshness_label(freshness: &RegistryFreshness) -> String {
    match (&freshness.current_version, &freshness.error) {
        (Some(version), _) => version_label(Some(version)),
        (None, Some(error)) => format!("unknown ({error})"),
        (None, None) => "-".to_string(),
    }
}

fn render_lifecycle_warning(ctx: &Ctx, freshness: &RegistryFreshness) {
    let Some(current) = &freshness.current else {
        return;
    };
    if current.yanked_at.is_some() {
        let reason = current
            .yank_reason
            .as_deref()
            .unwrap_or("no reason recorded");
        ctx.say(format!(
            "Registry lifecycle: current version is yanked ({reason})"
        ));
    } else if current.deprecated_at.is_some() {
        let reason = current
            .deprecation_reason
            .as_deref()
            .unwrap_or("no reason recorded");
        ctx.say(format!(
            "Registry lifecycle: current version is deprecated ({reason})"
        ));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    Direct,
    Stack,
}

impl Provenance {
    const fn as_str(self) -> &'static str {
        match self {
            Provenance::Direct => "direct",
            Provenance::Stack => "stack",
        }
    }
}

fn provenance(stacks: &[InstallVia]) -> Provenance {
    if stacks.is_empty() {
        Provenance::Direct
    } else {
        Provenance::Stack
    }
}

fn stack_label(via: &InstallVia) -> String {
    format!("{}/{}", via.org, via.stack)
}

fn remove_reason(stacks: &[InstallVia]) -> String {
    if stacks.is_empty() {
        "not required by any stack receipt".to_string()
    } else {
        format!(
            "still required by {} stack{}",
            stacks.len(),
            plural(stacks.len())
        )
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[derive(Serialize)]
struct InstalledListJson<'a> {
    installed: Vec<InstalledJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command_template: Option<String>,
}

#[derive(Serialize)]
struct InstalledStackListJson<'a> {
    installed: Vec<InstalledStackJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<String>,
}

#[derive(Serialize)]
struct InstalledAllListJson<'a> {
    skills: Vec<InstalledJson<'a>>,
    stacks: Vec<InstalledStackJson<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_message: Option<&'static str>,
}

#[derive(Serialize)]
struct InstalledInspectJson<'a> {
    receipt: &'a InstallReceipt,
    receipt_path: &'a Path,
    validation: InstalledValidationJson<'a>,
    hash_kind: Option<&'static str>,
    /// `true` if installed files drifted, `false` if they match the recorded
    /// package, `null` when there is no recorded hash to compare against.
    content_drifted: Option<bool>,
}

/// Render the `skill show --target` content-integrity line for a single skill.
fn render_content_drift_line(ctx: &Ctx, drift: &ContentDrift, receipt: &InstallReceipt) {
    let restore = drift_restore_command(receipt);
    match drift {
        ContentDrift::Matches => ctx.say("content: matches recorded package"),
        ContentDrift::Drifted { .. } => ctx.say(format!(
            "content: modified — installed files differ from recorded package (run `{restore}` to restore)"
        )),
        ContentDrift::Invalid { error } => ctx.say(format!(
            "content: modified — installed files are no longer a valid skill ({error}); run `{restore}` to restore"
        )),
        ContentDrift::Unknown => ctx.say("content: unknown (no recorded content hash)"),
    }
}

/// Command that reinstalls the recorded content for a drifted install.
/// Registry installs restore via `skill update --force`; local installs
/// (which `skill update` rejects) restore by reinstalling the source path.
fn drift_restore_command(receipt: &InstallReceipt) -> String {
    match receipt.source_type {
        ReceiptSourceType::Registry => format!(
            "agentstack skill update {} --target {} --force",
            receipt.skill_name, receipt.target
        ),
        ReceiptSourceType::Local => format!(
            "agentstack skill install {} --target {} --force",
            receipt.source_ref, receipt.target
        ),
    }
}

#[derive(Serialize)]
struct InstalledWhyJson<'a> {
    skill: &'a str,
    target: &'static str,
    source_type: &'static str,
    source_ref: &'a str,
    installed_version: Option<&'a str>,
    current_version_known: bool,
    current_version: Option<&'a str>,
    current_registry_version: Option<&'a str>,
    update_available: Option<bool>,
    registry_check_status: &'static str,
    registry_current: Option<&'a VersionInfo>,
    registry_check_error: Option<&'a str>,
    provenance: &'static str,
    required_by_stacks: Vec<String>,
    installed_by: InstalledByJson,
    direct_remove_safe: bool,
    safe_to_remove: bool,
    reason: String,
    next_command: String,
    receipt_path: &'a Path,
}

#[derive(Serialize)]
struct InstalledByJson {
    direct: bool,
    stacks: Vec<String>,
}

#[derive(Serialize)]
struct InstalledStackInspectJson<'a> {
    receipt: &'a StackInstallReceipt,
    receipt_path: &'a Path,
}

#[derive(Serialize)]
struct InstalledDoctorJson {
    target: &'static str,
    target_root: PathBuf,
    target_exists: bool,
    target_is_dir: bool,
    lock: TargetLockDiagnostics,
    staging_dirs: Vec<PathBuf>,
    receipts_parseable: usize,
    receipts_unreadable: usize,
    /// Installs whose files re-hash to their recorded content hash.
    recorded_package_matches: Vec<MatchedInstallJson>,
    /// Installs whose on-disk files drifted from the recorded content hash.
    drifted: Vec<DriftedInstallJson>,
    /// Installs that cannot be checked because no content hash was recorded
    /// (legacy receipts).
    unknown: Vec<UnknownInstallJson>,
    /// Registry lifecycle checks for registry-sourced installs (yanked,
    /// deprecated, or outdated versions). One `registry_lifecycle_skipped`
    /// entry when the registry could not be consulted.
    lifecycle: Vec<Check>,
}

#[derive(Serialize)]
struct MatchedInstallJson {
    skill: String,
}

#[derive(Serialize)]
struct DriftedInstallJson {
    skill: String,
    detail: String,
    restore_command: String,
}

#[derive(Serialize)]
struct UnknownInstallJson {
    skill: String,
    source: String,
    reason: String,
}

#[derive(Serialize)]
struct InstalledUnlockJson<'a> {
    target: &'static str,
    target_root: &'a Path,
    removed: bool,
    force: bool,
    lock: TargetLockDiagnostics,
}

#[derive(Serialize)]
struct InstalledValidationJson<'a> {
    ok: bool,
    errors: &'a [crate::skill::ValidationError],
}

#[derive(Serialize)]
struct InstalledJson<'a> {
    skill_name: &'a str,
    target: &'static str,
    source_type: &'static str,
    source_ref: &'a str,
    registry_url: Option<&'a str>,
    org: Option<&'a str>,
    version: Option<&'a str>,
    hash: Option<&'a str>,
    hash_kind: Option<&'static str>,
    installed_path: &'a Path,
    installed_at: &'a str,
    installed_by: Option<&'a str>,
    receipt: &'a Path,
}

#[derive(Serialize)]
struct InstalledStackJson<'a> {
    org: &'a str,
    stack: &'a str,
    target: &'static str,
    visibility: &'static str,
    team: Option<&'a str>,
    manifest_hash: &'a crate::package::PackageHash,
    resolved_at: &'a str,
    installed_at: &'a str,
    installed_by: Option<&'a str>,
    items: usize,
    receipt: &'a Path,
}

fn render_list_json(rows: &[InstalledRow], target: Option<InstallTarget>) -> Result<String> {
    let out = InstalledListJson {
        installed: installed_json_rows(rows),
        empty_message: rows.is_empty().then_some(installed_empty_message()),
        next_command: rows
            .is_empty()
            .then(|| installed_next_command(target))
            .filter(|command| is_concrete_next_command(command)),
        next_command_template: rows
            .is_empty()
            .then(|| installed_next_command(target))
            .filter(|command| !is_concrete_next_command(command)),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_stack_list_json(rows: &[InstalledStackRow]) -> Result<String> {
    let out = InstalledStackListJson {
        installed: installed_stack_json_rows(rows),
        empty_message: rows.is_empty().then_some(stack_installed_empty_message()),
        next_command: rows.is_empty().then(stack_installed_next_command).flatten(),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_all_list_json(
    skill_rows: &[InstalledRow],
    stack_rows: &[InstalledStackRow],
) -> Result<String> {
    let out = InstalledAllListJson {
        skills: installed_json_rows(skill_rows),
        stacks: installed_stack_json_rows(stack_rows),
        empty_message: (skill_rows.is_empty() && stack_rows.is_empty())
            .then_some(all_installed_empty_message()),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn installed_json_rows(rows: &[InstalledRow]) -> Vec<InstalledJson<'_>> {
    rows.iter()
        .map(|row| InstalledJson {
            skill_name: &row.receipt.skill_name,
            target: row.target.as_str(),
            source_type: row.receipt.source_type.as_str(),
            source_ref: &row.receipt.source_ref,
            registry_url: row.receipt.registry_url.as_deref(),
            org: row.receipt.org.as_deref(),
            version: row.receipt.version.as_deref(),
            hash: row.receipt.hash.as_deref(),
            hash_kind: row
                .receipt
                .hash
                .as_ref()
                .map(|_| row.receipt.source_type.hash_kind()),
            installed_path: &row.receipt.installed_path,
            installed_at: &row.receipt.installed_at,
            installed_by: row.receipt.installed_by.as_deref(),
            receipt: &row.receipt_path,
        })
        .collect()
}

fn installed_stack_json_rows(rows: &[InstalledStackRow]) -> Vec<InstalledStackJson<'_>> {
    rows.iter()
        .map(|row| InstalledStackJson {
            org: &row.receipt.org,
            stack: &row.receipt.stack,
            target: row.target.as_str(),
            visibility: row.receipt.visibility.as_str(),
            team: row.receipt.team.as_deref(),
            manifest_hash: &row.receipt.manifest_hash,
            resolved_at: &row.receipt.resolved_at,
            installed_at: &row.receipt.installed_at,
            installed_by: row.receipt.installed_by.as_deref(),
            items: row.receipt.items.len(),
            receipt: &row.receipt_path,
        })
        .collect()
}

fn installed_empty_message() -> &'static str {
    "no install receipts found."
}

fn installed_next_command(target: Option<InstallTarget>) -> String {
    format!(
        "agentstack skill install <path> --target {}",
        target.unwrap_or(InstallTarget::Local).as_str()
    )
}

fn is_concrete_next_command(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|first| first == "agentstack")
        && !(command.contains('<') || command.contains('>'))
}

fn stack_installed_empty_message() -> &'static str {
    "no stack install receipts found."
}

fn all_installed_empty_message() -> &'static str {
    "no installed skills or stacks found."
}

// Intentionally yields no next command: the empty stack-list view must not
// suggest `agentstack stack list` as a next step (a self-referential loop).
// Enforced by `install_receipts_stack_list_empty_json_*_next_command` tests.
fn stack_installed_next_command() -> Option<String> {
    None
}

fn source_label(receipt: &InstallReceipt) -> String {
    match receipt.source_type.as_str() {
        "registry" => receipt.source_ref.clone(),
        _ => "local".to_string(),
    }
}

fn short_hash(hash: &str) -> String {
    let Some((algorithm, hex)) = hash.split_once(':') else {
        return hash.chars().take(19).collect();
    };
    format!("{}:{}", algorithm, hex.chars().take(12).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn registry_check_error_classification_is_stable() {
        let unauthorized: anyhow::Error = CliError::new("unauthenticated", "not logged in")
            .action("authenticate")
            .into();
        assert_eq!(
            classify_registry_check_error(&unauthorized),
            RegistryCheckStatus::Unauthorized
        );

        let not_found: anyhow::Error = CliError::new("skill_not_found", "Skill not found")
            .action("list_versions")
            .http_status(404)
            .into();
        assert_eq!(
            classify_registry_check_error(&not_found),
            RegistryCheckStatus::NotFound
        );

        let unavailable = anyhow!("registry request failed");
        assert_eq!(
            classify_registry_check_error(&unavailable),
            RegistryCheckStatus::Unavailable
        );

        let invalid = anyhow!("registry install receipt is missing org");
        assert_eq!(
            classify_registry_check_error(&invalid),
            RegistryCheckStatus::InvalidReceipt
        );
    }

    #[test]
    fn provenance_is_receipt_referrer_based() {
        assert_eq!(provenance(&[]), Provenance::Direct);
        let stack = InstallVia {
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "engineering-default".to_string(),
            manifest_hash: "sha256:abc".to_string(),
        };
        assert_eq!(provenance(&[stack]), Provenance::Stack);
    }

    use crate::package::PackageHash;
    use crate::registry::{MockRegistryClient, SkillMetadata, Visibility};

    fn registry_receipt(skill: &str, version: &str) -> InstallReceipt {
        InstallReceipt {
            schema_version: crate::receipt::RECEIPT_SCHEMA_VERSION,
            skill_name: skill.to_string(),
            source_type: ReceiptSourceType::Registry,
            source_ref: format!("acme/{skill}"),
            registry_url: Some("https://registry.example".to_string()),
            org: Some("acme".to_string()),
            version: Some(version.to_string()),
            hash: None,
            content_hash: None,
            target: "local".to_string(),
            installed_path: PathBuf::from("/tmp/install"),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }
    }

    fn seed_version(mock: &MockRegistryClient, skill: &str, version: &str, current: bool) {
        mock.seed(
            SkillMetadata {
                name: skill.to_string(),
                description: format!("{skill} test skill"),
                org: "acme".to_string(),
                owner_email: None,
                team: None,
                visibility: Visibility::Org,
                version: version.to_string(),
                hash: PackageHash::sha256_of(version.as_bytes()),
                platform_tags: Vec::new(),
                created_at: Some("2026-01-01T00:00:00Z".to_string()),
                updated_at: Some("2026-01-01T00:00:00Z".to_string()),
                status: None,
                current: Some(current),
                yanked_at: None,
                yank_reason: None,
                deprecated_at: None,
                deprecation_reason: None,
                install_count: None,
                last_installed_at: None,
                audit_event_id: None,
            },
            Vec::new(),
        );
    }

    fn skill_ref(skill: &str) -> SkillRef {
        SkillRef::new("acme", skill).unwrap()
    }

    #[test]
    fn doctor_lifecycle_flags_yanked_install_as_fail() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        seed_version(&mock, "demo", "2", true);
        mock.yank(&skill_ref("demo"), "1", "credential leak")
            .unwrap();

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_YANKED);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].detail.contains("acme/demo@1"));
        assert!(checks[0].detail.contains("credential leak"));
        assert_eq!(
            checks[0].fix_command.as_deref(),
            Some("agentstack skill update demo --target local")
        );
    }

    #[test]
    fn doctor_lifecycle_yanked_without_current_suggests_uninstall() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        mock.yank(&skill_ref("demo"), "1", "broken").unwrap();

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_YANKED);
        assert_eq!(
            checks[0].fix_command.as_deref(),
            Some("agentstack skill uninstall demo --target local")
        );
    }

    #[test]
    fn doctor_lifecycle_flags_deprecated_install_as_warn() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        seed_version(&mock, "demo", "2", true);
        mock.deprecate(&skill_ref("demo"), "1", "use v2").unwrap();

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_DEPRECATED);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].detail.contains("use v2"));
        assert_eq!(
            checks[0].fix_command.as_deref(),
            Some("agentstack skill update demo --target local")
        );
    }

    #[test]
    fn doctor_lifecycle_healthy_installs_produce_no_checks() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", true);

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        assert!(checks.is_empty());
    }

    #[test]
    fn doctor_lifecycle_outdated_installs_summarized_once() {
        let mock = MockRegistryClient::new();
        for skill in ["alpha", "beta"] {
            seed_version(&mock, skill, "1", false);
            seed_version(&mock, skill, "2", true);
        }

        let receipts = [
            registry_receipt("alpha", "1"),
            registry_receipt("beta", "1"),
        ];
        let checks = registry_lifecycle_checks("local", &receipts, &mock);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_OUTDATED);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].detail.contains("2 installs"));
        assert_eq!(
            checks[0].fix_command.as_deref(),
            Some("agentstack install update --all")
        );
    }

    #[test]
    fn doctor_lifecycle_registry_errors_roll_up_into_single_skip_note() {
        let mock = MockRegistryClient::new();
        mock.fail_next_list_versions("registry request failed");

        let receipts = [
            registry_receipt("alpha", "1"),
            registry_receipt("beta", "1"),
        ];
        let checks = registry_lifecycle_checks("local", &receipts, &mock);
        assert_eq!(checks.len(), 1, "{checks:?}");
        assert_eq!(checks[0].code, CHECK_REGISTRY_LIFECYCLE_SKIPPED);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(
            checks[0]
                .detail
                .contains("registry lifecycle checks skipped")
        );
        assert!(checks[0].detail.contains("alpha"));
        assert!(checks[0].detail.contains("registry request failed"));
        // The failure does NOT halt further fetches: beta is still checked.
        assert_eq!(mock.list_versions_count(), 2);
    }

    #[test]
    fn doctor_lifecycle_caches_versions_per_skill() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", true);

        let receipts = [registry_receipt("demo", "1"), registry_receipt("demo", "1")];
        let checks = registry_lifecycle_checks("local", &receipts, &mock);
        assert!(checks.is_empty());
        assert_eq!(mock.list_versions_count(), 1);
    }

    #[test]
    fn doctor_lifecycle_json_check_codes_are_stable() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        seed_version(&mock, "demo", "2", true);
        mock.yank(&skill_ref("demo"), "1", "bad release").unwrap();

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        let json = serde_json::to_value(&checks).unwrap();
        assert_eq!(json[0]["code"].as_str(), Some("installed_version_yanked"));
        assert_eq!(json[0]["status"].as_str(), Some("fail"));
    }

    #[test]
    fn content_drift_matches_after_platform_overlay_install() {
        let scratch = std::env::temp_dir().join(format!(
            "agentstack-receipts-overlay-drift-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = scratch.join("alpha");
        let overlay = source.join("platform").join("claude-code");
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: alpha\ndescription: Use when alpha is needed\n---\n\n# Purpose\n\nBase.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();
        std::fs::write(
            overlay.join("SKILL.md"),
            "---\nname: alpha\ndescription: Use when alpha is needed\n---\n\n# Purpose\n\nOverlay.\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

        let report = crate::install::install_skill(crate::install::InstallOptions {
            source: &source,
            dest_root: &scratch.join("target"),
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(crate::receipt::InstallReceiptRequest {
                source_type: ReceiptSourceType::Registry,
                source_ref: "acme/alpha".to_string(),
                registry_url: Some("mock://registry".to_string()),
                org: Some("acme".to_string()),
                version: Some("1".to_string()),
                hash: None,
                content_hash: None,
                target: "claude-code".to_string(),
                installed_by: None,
                installed_via: None,
                installed_via_stacks: Vec::new(),
            }),
        })
        .unwrap();
        assert!(report.overlay.is_some(), "overlay should be applied");

        let receipt = crate::receipt::read_receipt_from_dir(&report.destination).unwrap();
        assert_eq!(
            content_drift(&report.destination, &receipt),
            ContentDrift::Matches,
            "a fresh overlay install must not be reported as drifted"
        );

        std::fs::write(report.destination.join("SKILL.md"), "edited").unwrap();
        assert!(
            content_drift(&report.destination, &receipt).is_drifted(),
            "local edits after install must still be detected"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn content_drift_detects_edits_to_local_install() {
        let scratch = std::env::temp_dir().join(format!(
            "agentstack-receipts-local-drift-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = scratch.join("alpha");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: alpha\ndescription: Use when alpha is needed\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n",
        )
        .unwrap();

        let report = crate::install::install_skill(crate::install::InstallOptions {
            source: &source,
            dest_root: &scratch.join("target"),
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(crate::receipt::InstallReceiptRequest {
                source_type: ReceiptSourceType::Local,
                source_ref: source.display().to_string(),
                registry_url: None,
                org: None,
                version: None,
                hash: None,
                content_hash: None,
                target: "local".to_string(),
                installed_by: None,
                installed_via: None,
                installed_via_stacks: Vec::new(),
            }),
        })
        .unwrap();

        let receipt = crate::receipt::read_receipt_from_dir(&report.destination).unwrap();
        assert_eq!(
            content_drift(&report.destination, &receipt),
            ContentDrift::Matches,
            "a fresh local install must not be reported as drifted"
        );

        std::fs::write(report.destination.join("SKILL.md"), "edited").unwrap();
        assert!(
            content_drift(&report.destination, &receipt).is_drifted(),
            "edits to a local install must be detected as drift"
        );

        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn doctor_lifecycle_continues_past_per_skill_errors() {
        let mock = MockRegistryClient::new();
        // `alpha` will fail its registry lookup; `beta` is yanked and MUST
        // still be flagged.
        seed_version(&mock, "beta", "1", false);
        seed_version(&mock, "beta", "2", true);
        mock.yank(&skill_ref("beta"), "1", "broken").unwrap();
        mock.fail_next_list_versions("registry request failed");

        let receipts = [
            registry_receipt("alpha", "1"),
            registry_receipt("beta", "1"),
        ];
        let checks = registry_lifecycle_checks("local", &receipts, &mock);
        assert_eq!(checks.len(), 2, "{checks:?}");
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_YANKED);
        assert_eq!(checks[0].name, "beta");
        assert_eq!(checks[1].code, CHECK_REGISTRY_LIFECYCLE_SKIPPED);
        assert!(checks[1].detail.contains("1 skill could not be checked"));
        assert!(checks[1].detail.contains("alpha"));
    }

    #[test]
    fn doctor_lifecycle_stack_owned_install_suggests_stack_update() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        seed_version(&mock, "demo", "2", true);
        mock.yank(&skill_ref("demo"), "1", "broken").unwrap();

        let mut receipt = registry_receipt("demo", "1");
        receipt.installed_via_stacks = vec![crate::receipt::InstallVia {
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "engineering-default".to_string(),
            manifest_hash: "deadbeef".to_string(),
        }];

        let checks = registry_lifecycle_checks("local", &[receipt], &mock);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].code, CHECK_INSTALLED_VERSION_YANKED);
        assert_eq!(
            checks[0].fix_command.as_deref(),
            Some("agentstack stack update acme/engineering-default --target local"),
            "stack-owned installs must not be told to run skill update"
        );
    }

    #[test]
    fn doctor_lifecycle_sanitizes_registry_controlled_reasons() {
        let mock = MockRegistryClient::new();
        seed_version(&mock, "demo", "1", false);
        mock.yank(&skill_ref("demo"), "1", "bad\u{1b}[31mred\u{1b}[0m\u{7}")
            .unwrap();

        let checks = registry_lifecycle_checks("local", &[registry_receipt("demo", "1")], &mock);
        assert_eq!(checks.len(), 1);
        assert!(
            !checks[0].detail.chars().any(|c| c.is_control()),
            "control characters must not reach the terminal: {:?}",
            checks[0].detail
        );
        assert!(checks[0].detail.contains("bad"));
    }
}
