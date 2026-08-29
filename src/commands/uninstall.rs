use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::ConfigStore;
use crate::error::CliError;
use crate::installed_scan::{InstalledStackRow, scan_installed, scan_installed_stacks};
use crate::output::Ctx;
use crate::receipt::{
    InstallReceipt, InstallVia, StackInstallReceipt, StackInstallReceiptItem, StackLookup,
    ensure_stack_receipt_dir_not_symlink, read_receipt_file, receipt_path, remove_stack_referrer,
    stack_referrers, validate_stack_receipt_item_paths, write_receipt_to_dir,
};
use crate::skill::check_slug;
use crate::targets::{InstallTarget, TargetResolver};

pub struct Args {
    pub subject: String,
    pub subject_name: Option<String>,
    pub target: Option<String>,
    pub force: bool,
    pub yes: bool,
    pub dry_run: bool,
}

enum UninstallSubject {
    Skill(String),
    Stack(String),
}

struct UninstallSelection {
    target: InstallTarget,
    installed_path: PathBuf,
}

#[derive(Serialize)]
struct UninstallPathJson<'a> {
    skill: &'a str,
    target: &'static str,
    path: &'a Path,
}

#[derive(Serialize)]
struct UninstallRemovedJson<'a> {
    removed: UninstallPathJson<'a>,
    source_type: Option<&'static str>,
    source_ref: Option<&'a str>,
    version: Option<&'a str>,
    hash: Option<&'a str>,
}

#[derive(Serialize)]
struct UninstallDryRunJson<'a> {
    would_remove: UninstallPathJson<'a>,
    source_type: Option<&'static str>,
    source_ref: Option<&'a str>,
    version: Option<&'a str>,
    hash: Option<&'a str>,
    dry_run: bool,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let subject = parse_subject(&args.subject, args.subject_name.as_deref())?;
    match subject {
        UninstallSubject::Skill(skill_name) => run_skill(ctx, args, skill_name),
        UninstallSubject::Stack(stack) => run_stack(ctx, args, stack),
    }
}

fn parse_subject(subject: &str, subject_name: Option<&str>) -> Result<UninstallSubject> {
    match (subject, subject_name) {
        ("skill", Some(name)) => Ok(UninstallSubject::Skill(name.to_string())),
        ("stack", Some(name)) => Ok(UninstallSubject::Stack(name.to_string())),
        ("skill", None) => {
            bail!("`agentstack skill uninstall <skill>` requires a skill name")
        }
        ("stack", None) => {
            bail!("`agentstack stack uninstall <stack>` requires a stack name")
        }
        (_, Some(_)) => {
            bail!("unknown uninstall kind `{subject}` (expected `skill` or `stack`)")
        }
        (name, None) => Ok(UninstallSubject::Skill(name.to_string())),
    }
}

fn run_skill(ctx: &Ctx, args: Args, skill_name: String) -> Result<()> {
    check_slug(&skill_name)
        .map_err(|reason| anyhow::anyhow!("invalid skill name `{}`: {reason}", skill_name))?;

    let selection = resolve_selection(ctx, &skill_name, args.target.as_deref())?;
    ensure_installed_path_exists(&skill_name, selection.target, &selection.installed_path)?;

    let receipt_file = receipt_path(&selection.installed_path);
    let receipt = load_receipt(
        ctx,
        &skill_name,
        selection.target,
        &receipt_file,
        args.force,
    )?;
    if let Some(receipt) = &receipt
        && receipt.installed_path != selection.installed_path
    {
        if !args.force {
            bail!(
                "refusing to remove skill `{}`: receipt installed_path `{}` does not match resolved path `{}`; rerun with --force to remove the resolved path anyway",
                skill_name,
                receipt.installed_path.display(),
                selection.installed_path.display()
            );
        }
        warn(
            ctx,
            format!(
                "receipt installed_path `{}` does not match resolved path `{}`; removing the resolved path because --force was set",
                receipt.installed_path.display(),
                selection.installed_path.display()
            ),
        );
    }
    if let Some(receipt) = &receipt {
        ensure_not_stack_owned_child(receipt, &skill_name, selection.target)?;
    }

    if args.dry_run {
        return report_dry_run(
            ctx,
            &skill_name,
            selection.target,
            &selection.installed_path,
            receipt.as_ref(),
        );
    }

    if !args.yes
        && confirm_removal(
            ctx,
            format!(
                "Remove skill `{skill_name}` from target `{}` at {}?",
                selection.target.as_str(),
                selection.installed_path.display()
            ),
        )? == ConfirmRemove::Declined
    {
        ctx.say("no changes made");
        return Ok(());
    }

    fs::remove_dir_all(&selection.installed_path)
        .with_context(|| format!("failed to remove `{}`", selection.installed_path.display()))?;

    report_removed(
        ctx,
        &skill_name,
        selection.target,
        &selection.installed_path,
        receipt.as_ref(),
    )
}

fn resolve_selection(
    ctx: &Ctx,
    skill_name: &str,
    target_name: Option<&str>,
) -> Result<UninstallSelection> {
    if let Some(target_name) = target_name {
        let target = InstallTarget::parse(target_name)?;
        let store = ConfigStore::load().context("failed to load config")?;
        let resolver = TargetResolver::new(&store);
        let resolved = resolver.resolve(target)?;
        return Ok(UninstallSelection {
            target,
            installed_path: resolved.path.join(skill_name),
        });
    }

    let matches: Vec<_> = scan_installed(|receipt_file, e| {
        if !ctx.json && !ctx.quiet {
            eprintln!(
                "warning: skipping unreadable install receipt `{}`: {e}",
                receipt_file.display()
            );
        }
    })?
    .into_iter()
    .filter(|row| row.receipt.skill_name == skill_name)
    .collect();

    match matches.len() {
        0 => Err(skill_not_installed_error(
            skill_name,
            format!("skill `{skill_name}` is not installed in any configured target"),
            None,
        )
        .into()),
        1 => {
            let row = matches.into_iter().next().expect("match count is one");
            Ok(UninstallSelection {
                target: row.target,
                installed_path: row.installed_path,
            })
        }
        _ => {
            let targets = matches
                .iter()
                .map(|row| row.target.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "skill `{skill_name}` is installed in multiple targets: {targets}; specify --target"
            )
        }
    }
}

fn ensure_installed_path_exists(
    skill_name: &str,
    target: InstallTarget,
    installed_path: &Path,
) -> Result<()> {
    match fs::metadata(installed_path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(skill_not_installed_error(
            skill_name,
            format!(
                "skill `{skill_name}` is not installed at {} (path exists but is not a directory)",
                installed_path.display()
            ),
            Some(target),
        )
        .into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(skill_not_installed_error(
            skill_name,
            format!(
                "skill `{skill_name}` is not installed at {}",
                installed_path.display()
            ),
            Some(target),
        )
        .into()),
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to inspect installed skill path `{}`",
                installed_path.display()
            )
        }),
    }
}

fn skill_not_installed_error(
    skill_name: &str,
    message: String,
    target: Option<InstallTarget>,
) -> CliError {
    let next_command = match target {
        Some(target) => format!("agentstack install list --target {}", target.as_str()),
        None => "agentstack install list".to_string(),
    };
    CliError::new("install_receipt_missing", message)
        .resource(skill_name)
        .action("uninstall")
        .next_command(next_command)
}

fn load_receipt(
    ctx: &Ctx,
    skill_name: &str,
    target: InstallTarget,
    receipt_file: &Path,
    force: bool,
) -> Result<Option<InstallReceipt>> {
    if !receipt_file.is_file() {
        if force {
            warn(
                ctx,
                format!(
                    "no install receipt found at `{}`; proceeding because --force was set",
                    receipt_file.display()
                ),
            );
            return Ok(None);
        }
        return Err(CliError::new(
            "install_receipt_missing",
            format!(
                "refusing to remove skill `{skill_name}` from target `{}` because no install receipt was found at `{}`; rerun with --force to remove the directory anyway",
                target.as_str(),
                receipt_file.display()
            ),
        )
        .resource(skill_name)
        .action("uninstall")
        .next_command(format!(
            "agentstack skill uninstall {skill_name} --target {} --force --yes",
            target.as_str()
        ))
        .into());
    }

    match read_receipt_file(receipt_file) {
        Ok(receipt) => Ok(Some(receipt)),
        Err(err) if force => {
            warn(
                ctx,
                format!(
                    "install receipt `{}` could not be read: {err}; proceeding because --force was set",
                    receipt_file.display()
                ),
            );
            Ok(None)
        }
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to read install receipt for skill `{skill_name}` at `{}`; rerun with --force to remove the directory anyway",
                receipt_file.display()
            )
        }),
    }
}

struct StackUninstallPlan {
    row: InstalledStackRow,
    items: Vec<StackUninstallItem>,
}

struct StackUninstallItem {
    skill: String,
    path: PathBuf,
    action: StackUninstallAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StackUninstallAction {
    RemoveSkill,
    KeepShared { remaining_stacks: Vec<InstallVia> },
    KeepForeign { owner: String },
    KeepUnverified { reason: String },
    Missing,
}

#[derive(Serialize)]
struct StackUninstallJson<'a> {
    kind: &'static str,
    org: &'a str,
    stack: &'a str,
    target: &'static str,
    receipt: &'a Path,
    dry_run: bool,
    items: Vec<StackUninstallItemJson<'a>>,
    summary: StackUninstallSummary,
}

#[derive(Serialize)]
struct StackUninstallItemJson<'a> {
    skill: &'a str,
    path: &'a Path,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct StackUninstallSummary {
    removed: usize,
    kept_shared: usize,
    kept_foreign: usize,
    left_in_place: usize,
    missing: usize,
}

fn run_stack(ctx: &Ctx, args: Args, stack: String) -> Result<()> {
    let row = resolve_stack_selection(ctx, &stack, args.target.as_deref())?;
    let plan = build_stack_uninstall_plan(row, args.force)?;

    if args.dry_run {
        return report_stack_plan(ctx, &plan, true);
    }

    if !args.yes
        && confirm_removal(
            ctx,
            format!(
                "Remove stack `{}/{}` from target `{}`?",
                plan.row.receipt.org,
                plan.row.receipt.stack,
                plan.row.target.as_str()
            ),
        )? == ConfirmRemove::Declined
    {
        ctx.say("no changes made");
        return Ok(());
    }

    apply_stack_uninstall_plan(&plan)?;
    report_stack_plan(ctx, &plan, false)
}

fn resolve_stack_selection(
    ctx: &Ctx,
    stack: &str,
    target_name: Option<&str>,
) -> Result<InstalledStackRow> {
    let lookup = StackLookup::parse(stack)?;
    let target_filter = target_name.map(InstallTarget::parse).transpose()?;
    let matches: Vec<_> = scan_installed_stacks(|receipt_file, e| {
        if !ctx.json && !ctx.quiet {
            eprintln!(
                "warning: skipping unreadable stack receipt `{}`: {e}",
                receipt_file.display()
            );
        }
    })?
    .into_iter()
    .filter(|row| row.receipt.stack == lookup.stack)
    .filter(|row| {
        lookup
            .org
            .as_deref()
            .is_none_or(|org| row.receipt.org == org)
    })
    .filter(|row| target_filter.is_none_or(|target| row.target == target))
    .collect();

    match matches.len() {
        0 => Err(stack_not_installed_error(lookup.label(), target_filter).into()),
        1 => Ok(matches.into_iter().next().expect("match count is one")),
        _ => {
            let choices = matches
                .iter()
                .map(|row| {
                    format!(
                        "{}/{} in {}",
                        row.receipt.org,
                        row.receipt.stack,
                        row.target.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if target_filter.is_some() {
                bail!(
                    "multiple stack install receipts named `{}` found: {choices}; remove the duplicate receipt or use a unique stack slug",
                    lookup.label()
                );
            }
            bail!(
                "stack `{}` is installed multiple times: {choices}; specify --target",
                lookup.label()
            )
        }
    }
}

fn stack_not_installed_error(stack: String, target: Option<InstallTarget>) -> CliError {
    match target {
        Some(target) => CliError::new(
            "stack_receipt_missing",
            format!(
                "stack `{stack}` is not installed in target `{}`",
                target.as_str()
            ),
        )
        .resource(stack)
        .action("uninstall")
        .next_command(format!(
            "agentstack install list --kind stack --target {}",
            target.as_str()
        )),
        None => CliError::new(
            "stack_receipt_missing",
            format!("stack `{stack}` is not installed in any configured target"),
        )
        .resource(stack)
        .action("uninstall")
        .next_command("agentstack install list --kind stack"),
    }
}

fn build_stack_uninstall_plan(row: InstalledStackRow, force: bool) -> Result<StackUninstallPlan> {
    let mut items = Vec::new();
    for item in &row.receipt.items {
        validate_stack_receipt_item_paths(&row.target_root, item)?;
        let action = classify_stack_child(&row.receipt, item, force)?;
        items.push(StackUninstallItem {
            skill: item.skill.clone(),
            path: item.install_path.clone(),
            action,
        });
    }
    Ok(StackUninstallPlan { row, items })
}

fn classify_stack_child(
    stack_receipt: &StackInstallReceipt,
    item: &StackInstallReceiptItem,
    force: bool,
) -> Result<StackUninstallAction> {
    if !item.install_path.exists() {
        return Ok(StackUninstallAction::Missing);
    }
    if !item.installed_receipt_path.is_file() {
        if force {
            return Ok(StackUninstallAction::KeepUnverified {
                reason: format!(
                    "no install receipt at `{}`",
                    item.installed_receipt_path.display()
                ),
            });
        }
        bail!(
            "refusing to remove stack child `{}`: no install receipt found at `{}`; rerun with --force to uninstall the stack and leave the directory in place",
            item.skill,
            item.installed_receipt_path.display()
        );
    }
    let receipt = match read_receipt_file(&item.installed_receipt_path) {
        Ok(receipt) => receipt,
        Err(_) if force => {
            return Ok(StackUninstallAction::KeepUnverified {
                reason: format!(
                    "install receipt `{}` is unreadable; inspect or remove the directory manually",
                    item.installed_receipt_path.display()
                ),
            });
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "failed to read install receipt for stack child `{}`; rerun with --force to uninstall the stack and leave the directory in place",
                    item.skill
                )
            });
        }
    };
    let refs = stack_referrers(&receipt);
    let owns_child = refs
        .iter()
        .any(|via| via.org == stack_receipt.org && via.stack == stack_receipt.stack);
    if !owns_child {
        return Ok(StackUninstallAction::KeepForeign {
            owner: child_owner_label(&receipt),
        });
    }
    let remaining = refs
        .into_iter()
        .filter(|via| !(via.org == stack_receipt.org && via.stack == stack_receipt.stack))
        .collect::<Vec<_>>();
    if remaining.is_empty() {
        Ok(StackUninstallAction::RemoveSkill)
    } else {
        Ok(StackUninstallAction::KeepShared {
            remaining_stacks: remaining,
        })
    }
}

fn apply_stack_uninstall_plan(plan: &StackUninstallPlan) -> Result<()> {
    for item in &plan.items {
        match &item.action {
            StackUninstallAction::RemoveSkill => {
                if item.path.exists() {
                    fs::remove_dir_all(&item.path)
                        .with_context(|| format!("failed to remove `{}`", item.path.display()))?;
                }
            }
            StackUninstallAction::KeepShared { .. } => {
                let mut receipt = read_receipt_file(&receipt_path(&item.path))?;
                let mut refs = stack_referrers(&receipt);
                remove_stack_referrer(&mut refs, &plan.row.receipt.org, &plan.row.receipt.stack);
                receipt.installed_via_stacks = refs;
                receipt.installed_via = receipt.installed_via_stacks.first().cloned();
                write_receipt_to_dir(&item.path, &receipt)?;
            }
            StackUninstallAction::KeepForeign { .. }
            | StackUninstallAction::KeepUnverified { .. }
            | StackUninstallAction::Missing => {}
        }
    }
    remove_stack_receipt(&plan.row.receipt_path)
}

fn remove_stack_receipt(receipt_path: &Path) -> Result<()> {
    let stack_dir = receipt_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("stack receipt has no parent directory"))?;
    let org_dir = stack_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("stack receipt organization has no parent directory"))?;
    let stacks_root = org_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("stack receipt root has no parent directory"))?;
    ensure_stack_receipt_dir_not_symlink(stacks_root)?;
    ensure_stack_receipt_dir_not_symlink(org_dir)?;
    ensure_stack_receipt_dir_not_symlink(stack_dir)?;
    fs::remove_dir_all(stack_dir)
        .with_context(|| format!("failed to remove `{}`", stack_dir.display()))?;
    if fs::read_dir(org_dir)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir(org_dir);
        if fs::read_dir(stacks_root)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(stacks_root);
        }
    }
    Ok(())
}

fn ensure_not_stack_owned_child(
    receipt: &InstallReceipt,
    skill_name: &str,
    target: InstallTarget,
) -> Result<()> {
    let refs = stack_referrers(receipt);
    if refs.is_empty() {
        return Ok(());
    }
    let stacks = refs
        .iter()
        .map(|via| format!("`{}/{}`", via.org, via.stack))
        .collect::<Vec<_>>()
        .join(", ");
    let command = if refs.len() == 1 {
        format!(
            "agentstack stack uninstall {}/{} --target {}",
            refs[0].org,
            refs[0].stack,
            target.as_str()
        )
    } else {
        format!(
            "agentstack stack uninstall <org>/<stack> --target {}",
            target.as_str()
        )
    };
    bail!(
        "cannot remove stack-owned child skill `{skill_name}` directly; it is referenced by {stacks}; run `{command}`"
    )
}

fn report_stack_plan(ctx: &Ctx, plan: &StackUninstallPlan, dry_run: bool) -> Result<()> {
    let summary = stack_uninstall_summary(plan);
    if ctx.json {
        let payload = StackUninstallJson {
            kind: "stack",
            org: &plan.row.receipt.org,
            stack: &plan.row.receipt.stack,
            target: plan.row.target.as_str(),
            receipt: &plan.row.receipt_path,
            dry_run,
            items: plan.items.iter().map(stack_uninstall_item_json).collect(),
            summary,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let verb = if dry_run {
        "would uninstall"
    } else {
        "uninstalled"
    };
    ctx.say(format!(
        "{verb} stack `{}/{}` from target `{}`",
        plan.row.receipt.org,
        plan.row.receipt.stack,
        plan.row.target.as_str()
    ));
    if ctx.quiet {
        return Ok(());
    }
    ctx.say(format!(
        "  removed child skills: {} | kept shared: {} | kept foreign: {} | left in place: {} | missing: {}",
        summary.removed,
        summary.kept_shared,
        summary.kept_foreign,
        summary.left_in_place,
        summary.missing
    ));
    for item in &plan.items {
        match &item.action {
            StackUninstallAction::RemoveSkill => {
                ctx.say(format!("  remove: {}", item.skill));
            }
            StackUninstallAction::KeepShared { remaining_stacks } => {
                ctx.say(format!(
                    "  keep:   {} (still referenced by {})",
                    item.skill,
                    stack_list(remaining_stacks)
                ));
            }
            StackUninstallAction::KeepForeign { owner } => {
                ctx.say(format!("  keep:   {} ({owner})", item.skill));
            }
            StackUninstallAction::KeepUnverified { reason } => {
                ctx.say(format!("  left in place: {} ({reason})", item.skill));
            }
            StackUninstallAction::Missing => {
                ctx.say(format!("  missing: {}", item.skill));
            }
        }
    }
    if dry_run {
        ctx.say("dry run; nothing removed.");
    } else if summary.removed > 0 {
        ctx.say("removed child skills are already gone; no separate skill uninstall is needed.");
    }
    Ok(())
}

fn stack_uninstall_item_json(item: &StackUninstallItem) -> StackUninstallItemJson<'_> {
    match &item.action {
        StackUninstallAction::RemoveSkill => StackUninstallItemJson {
            skill: &item.skill,
            path: &item.path,
            action: "remove",
            reason: None,
        },
        StackUninstallAction::KeepShared { remaining_stacks } => StackUninstallItemJson {
            skill: &item.skill,
            path: &item.path,
            action: "keep_shared",
            reason: Some(format!(
                "still referenced by {}",
                stack_list(remaining_stacks)
            )),
        },
        StackUninstallAction::KeepForeign { owner } => StackUninstallItemJson {
            skill: &item.skill,
            path: &item.path,
            action: "keep_foreign",
            reason: Some(owner.clone()),
        },
        StackUninstallAction::KeepUnverified { reason } => StackUninstallItemJson {
            skill: &item.skill,
            path: &item.path,
            action: "left_in_place",
            reason: Some(reason.clone()),
        },
        StackUninstallAction::Missing => StackUninstallItemJson {
            skill: &item.skill,
            path: &item.path,
            action: "missing",
            reason: None,
        },
    }
}

fn stack_uninstall_summary(plan: &StackUninstallPlan) -> StackUninstallSummary {
    StackUninstallSummary {
        removed: plan
            .items
            .iter()
            .filter(|item| matches!(item.action, StackUninstallAction::RemoveSkill))
            .count(),
        kept_shared: plan
            .items
            .iter()
            .filter(|item| matches!(item.action, StackUninstallAction::KeepShared { .. }))
            .count(),
        kept_foreign: plan
            .items
            .iter()
            .filter(|item| matches!(item.action, StackUninstallAction::KeepForeign { .. }))
            .count(),
        left_in_place: plan
            .items
            .iter()
            .filter(|item| matches!(item.action, StackUninstallAction::KeepUnverified { .. }))
            .count(),
        missing: plan
            .items
            .iter()
            .filter(|item| matches!(item.action, StackUninstallAction::Missing))
            .count(),
    }
}

fn child_owner_label(receipt: &InstallReceipt) -> String {
    let refs = stack_referrers(receipt);
    if !refs.is_empty() {
        return format!("owned by {}", stack_list(&refs));
    }
    match &receipt.installed_via {
        Some(via) => format!("owned by {} provenance", via.kind),
        None => "not owned by this stack".to_string(),
    }
}

fn stack_list(stacks: &[InstallVia]) -> String {
    stacks
        .iter()
        .map(|via| format!("stack `{}/{}`", via.org, via.stack))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmRemove {
    Accepted,
    Declined,
}

fn confirm_removal(ctx: &Ctx, prompt: String) -> Result<ConfirmRemove> {
    if !ctx.can_prompt() {
        bail!("uninstall requires --yes when stdin/stderr is not a TTY");
    }

    eprint!("{prompt} [y/N] ");
    if io::stderr().flush().is_err() {
        return Ok(ConfirmRemove::Declined);
    }

    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(ConfirmRemove::Accepted)
    } else {
        Ok(ConfirmRemove::Declined)
    }
}

fn report_removed(
    ctx: &Ctx,
    skill_name: &str,
    target: InstallTarget,
    installed_path: &Path,
    receipt: Option<&InstallReceipt>,
) -> Result<()> {
    if ctx.json {
        let payload = UninstallRemovedJson {
            removed: UninstallPathJson {
                skill: skill_name,
                target: target.as_str(),
                path: installed_path,
            },
            source_type: receipt.map(|receipt| receipt.source_type.as_str()),
            source_ref: receipt.map(|receipt| receipt.source_ref.as_str()),
            version: receipt.and_then(|receipt| receipt.version.as_deref()),
            hash: receipt.and_then(|receipt| receipt.hash.as_deref()),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!(
        "removed skill `{skill_name}` from target `{}` at {}",
        target.as_str(),
        installed_path.display()
    ));
    Ok(())
}

fn report_dry_run(
    ctx: &Ctx,
    skill_name: &str,
    target: InstallTarget,
    installed_path: &Path,
    receipt: Option<&InstallReceipt>,
) -> Result<()> {
    if ctx.json {
        let payload = UninstallDryRunJson {
            would_remove: UninstallPathJson {
                skill: skill_name,
                target: target.as_str(),
                path: installed_path,
            },
            source_type: receipt.map(|receipt| receipt.source_type.as_str()),
            source_ref: receipt.map(|receipt| receipt.source_ref.as_str()),
            version: receipt.and_then(|receipt| receipt.version.as_deref()),
            hash: receipt.and_then(|receipt| receipt.hash.as_deref()),
            dry_run: true,
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!(
        "would remove skill `{skill_name}` from target `{}` at {}",
        target.as_str(),
        installed_path.display()
    ));
    ctx.say("dry run; nothing removed.");
    Ok(())
}

fn warn(ctx: &Ctx, message: impl AsRef<str>) {
    if !ctx.quiet {
        eprintln!("warning: {}", message.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_fs::TempDir;

    use crate::package::PackageHash;
    use crate::receipt::ReceiptSourceType;
    use crate::receipt::{
        RECEIPT_SCHEMA_VERSION, read_receipt_from_dir, write_receipt_to_dir, write_stack_receipt,
    };
    use crate::registry::Visibility;

    fn hash(hex: &str) -> PackageHash {
        PackageHash {
            algorithm: "sha256".to_string(),
            hex: hex.to_string(),
        }
    }

    fn via(stack: &str) -> InstallVia {
        InstallVia {
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: stack.to_string(),
            manifest_hash: format!("manifest-{stack}"),
        }
    }

    #[test]
    fn stack_lookup_accepts_org_qualified_refs() {
        let lookup = StackLookup::parse("acme/engineering-default").unwrap();
        assert_eq!(lookup.org.as_deref(), Some("acme"));
        assert_eq!(lookup.stack, "engineering-default");
        assert_eq!(lookup.label(), "acme/engineering-default");

        let bare = StackLookup::parse("engineering-default").unwrap();
        assert_eq!(bare.org, None);
        assert_eq!(bare.stack, "engineering-default");
        assert_eq!(bare.label(), "engineering-default");
    }

    #[test]
    fn stack_uninstall_keeps_shared_child_and_removes_only_this_referrer() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let child_dir = target_root.join("shared-skill");
        fs::create_dir_all(&child_dir).unwrap();

        let stack_a = via("stack-a");
        let stack_b = via("stack-b");
        let child_receipt = InstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            skill_name: "shared-skill".to_string(),
            source_type: ReceiptSourceType::Registry,
            source_ref: "acme/shared-skill".to_string(),
            registry_url: Some("mock://registry".to_string()),
            org: Some("acme".to_string()),
            version: Some("1".to_string()),
            hash: Some("sha256:abc".to_string()),
            content_hash: Some("sha256:abc".to_string()),
            target: "local".to_string(),
            installed_path: child_dir.clone(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            installed_via: Some(stack_b.clone()),
            installed_via_stacks: vec![stack_a.clone(), stack_b.clone()],
        };
        write_receipt_to_dir(&child_dir, &child_receipt).unwrap();

        let stack_receipt = StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "stack-a".to_string(),
            registry_url: Some("mock://registry".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: hash("manifest-a"),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            items: vec![StackInstallReceiptItem {
                skill: "shared-skill".to_string(),
                version_id: "1".to_string(),
                version: "1".to_string(),
                archive_hash: hash("abc"),
                install_path: child_dir.clone(),
                installed_receipt_path: receipt_path(&child_dir),
            }],
        };
        let stack_receipt_path = write_stack_receipt(&target_root, &stack_receipt).unwrap();
        let row = InstalledStackRow {
            target: InstallTarget::Local,
            target_root: target_root.clone(),
            receipt_path: stack_receipt_path.clone(),
            receipt: stack_receipt,
        };

        let plan = build_stack_uninstall_plan(row, false).unwrap();
        assert!(matches!(
            plan.items[0].action,
            StackUninstallAction::KeepShared { .. }
        ));
        apply_stack_uninstall_plan(&plan).unwrap();

        assert!(child_dir.is_dir());
        assert!(!stack_receipt_path.exists());
        let receipt = read_receipt_from_dir(&child_dir).unwrap();
        let refs = stack_referrers(&receipt);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].stack, "stack-b");
    }

    #[test]
    fn stack_receipt_cleanup_preserves_non_empty_stacks_root() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");

        let stack_a = StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "stack-a".to_string(),
            registry_url: Some("mock://registry".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: hash("manifest-a"),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            items: Vec::new(),
        };
        let stack_b = StackInstallReceipt {
            stack: "stack-b".to_string(),
            manifest_hash: hash("manifest-b"),
            ..stack_a.clone()
        };
        let stack_a_path = write_stack_receipt(&target_root, &stack_a).unwrap();
        let stack_b_path = write_stack_receipt(&target_root, &stack_b).unwrap();

        remove_stack_receipt(&stack_a_path).unwrap();

        assert!(!stack_a_path.exists());
        assert!(stack_b_path.is_file());
        assert!(target_root.join(".agentstack-stacks").is_dir());
        assert!(target_root.join(".agentstack-stacks/acme").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn stack_receipt_cleanup_refuses_symlinked_stacks_root() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let outside_stack = tmp.path().join("outside/acme/stack-a");
        fs::create_dir_all(&outside_stack).unwrap();
        let outside_receipt = outside_stack.join(".agentstack.json");
        let outside_marker = outside_stack.join("marker");
        fs::write(&outside_receipt, b"receipt").unwrap();
        fs::write(&outside_marker, b"keep").unwrap();
        fs::create_dir_all(&target_root).unwrap();
        symlink(
            tmp.path().join("outside"),
            target_root.join(".agentstack-stacks"),
        )
        .unwrap();

        let receipt_path = target_root.join(".agentstack-stacks/acme/stack-a/.agentstack.json");
        let err = remove_stack_receipt(&receipt_path).unwrap_err();
        let message = format!("{err:#}");

        assert!(
            message.contains("is a symlink; refusing"),
            "message = {message}"
        );
        assert!(outside_receipt.is_file());
        assert!(outside_marker.is_file());
    }

    fn stack_receipt_for_child(child_dir: &Path) -> StackInstallReceipt {
        StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "stack-a".to_string(),
            registry_url: Some("mock://registry".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: hash("manifest-a"),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            items: vec![StackInstallReceiptItem {
                skill: "orphan-skill".to_string(),
                version_id: "1".to_string(),
                version: "1".to_string(),
                archive_hash: hash("abc"),
                install_path: child_dir.to_path_buf(),
                installed_receipt_path: receipt_path(child_dir),
            }],
        }
    }

    #[test]
    fn stack_uninstall_force_leaves_child_without_receipt_in_place() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let child_dir = target_root.join("orphan-skill");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(child_dir.join("SKILL.md"), "# Orphan\n").unwrap();
        // No child install receipt: agentstack has no proof it owns this path.

        let stack_receipt = stack_receipt_for_child(&child_dir);
        let stack_receipt_path = write_stack_receipt(&target_root, &stack_receipt).unwrap();
        let row = InstalledStackRow {
            target: InstallTarget::Local,
            target_root: target_root.clone(),
            receipt_path: stack_receipt_path.clone(),
            receipt: stack_receipt,
        };

        let plan = build_stack_uninstall_plan(row, true).unwrap();
        match &plan.items[0].action {
            StackUninstallAction::KeepUnverified { reason } => {
                assert!(reason.contains("no install receipt at"), "reason: {reason}");
            }
            other => panic!("expected KeepUnverified, got {other:?}"),
        }
        apply_stack_uninstall_plan(&plan).unwrap();

        assert!(
            child_dir.is_dir(),
            "child without an install receipt must be left in place"
        );
        assert!(child_dir.join("SKILL.md").is_file());
        assert!(!stack_receipt_path.exists());
    }

    #[test]
    fn stack_uninstall_force_leaves_child_with_unreadable_receipt_in_place() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let child_dir = target_root.join("orphan-skill");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(child_dir.join("SKILL.md"), "# Orphan\n").unwrap();
        fs::write(receipt_path(&child_dir), "{ not json").unwrap();

        let stack_receipt = stack_receipt_for_child(&child_dir);
        let stack_receipt_path = write_stack_receipt(&target_root, &stack_receipt).unwrap();
        let row = InstalledStackRow {
            target: InstallTarget::Local,
            target_root: target_root.clone(),
            receipt_path: stack_receipt_path.clone(),
            receipt: stack_receipt,
        };

        let plan = build_stack_uninstall_plan(row, true).unwrap();
        match &plan.items[0].action {
            StackUninstallAction::KeepUnverified { reason } => {
                assert!(reason.contains("is unreadable"), "reason: {reason}");
            }
            other => panic!("expected KeepUnverified, got {other:?}"),
        }
        apply_stack_uninstall_plan(&plan).unwrap();

        assert!(
            child_dir.is_dir(),
            "child with an unreadable install receipt must be left in place"
        );
        assert!(child_dir.join("SKILL.md").is_file());
        assert!(receipt_path(&child_dir).is_file());
        assert!(!stack_receipt_path.exists());
    }

    #[test]
    fn stack_uninstall_force_rejects_child_path_outside_target_root() {
        let tmp = TempDir::new().unwrap();
        let target_root = tmp.path().join("target");
        let outside_dir = tmp.path().join("outside-skill");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("SKILL.md"), "# Outside\n").unwrap();

        let stack_receipt = StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "stack-a".to_string(),
            registry_url: Some("mock://registry".to_string()),
            visibility: Visibility::Org,
            team: None,
            resolved_at: "2026-01-01T00:00:00Z".to_string(),
            manifest_hash: hash("manifest-a"),
            target: "local".to_string(),
            installed_at: "2026-01-01T00:00:00Z".to_string(),
            installed_by: Some("octocat".to_string()),
            items: vec![StackInstallReceiptItem {
                skill: "outside-skill".to_string(),
                version_id: "1".to_string(),
                version: "1".to_string(),
                archive_hash: hash("abc"),
                install_path: outside_dir.clone(),
                installed_receipt_path: receipt_path(&outside_dir),
            }],
        };
        let row = InstalledStackRow {
            target: InstallTarget::Local,
            target_root,
            receipt_path: tmp.path().join(".agentstack.json"),
            receipt: stack_receipt,
        };

        let err = match build_stack_uninstall_plan(row, true) {
            Ok(_) => panic!("forged outside-root stack child path was accepted"),
            Err(err) => err,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("outside target root"), "msg: {msg}");
        assert!(outside_dir.is_dir());
        assert!(outside_dir.join("SKILL.md").is_file());
    }
}
