//! Shared scanning for AgentStack install receipts.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::ConfigStore;
use crate::receipt::{
    InstallReceipt, RECEIPT_FILE, STACK_RECEIPT_FILE, StackInstallReceipt,
    ensure_stack_receipt_dir_not_symlink, read_receipt_file, read_stack_receipt_file,
};
use crate::targets::{InstallTarget, TargetResolver};

#[derive(Debug, Clone)]
pub struct InstalledRow {
    pub target: InstallTarget,
    pub installed_path: PathBuf,
    pub receipt_path: PathBuf,
    pub receipt: InstallReceipt,
}

#[derive(Debug, Clone)]
pub struct InstalledStackRow {
    pub target: InstallTarget,
    pub target_root: PathBuf,
    pub receipt_path: PathBuf,
    pub receipt: StackInstallReceipt,
}

pub fn scan_installed<F>(mut on_unreadable_receipt: F) -> Result<Vec<InstalledRow>>
where
    F: FnMut(&Path, &anyhow::Error),
{
    let mut rows = Vec::new();
    for_each_target_root(|target, root| {
        scan_target(target, root, &mut rows, &mut on_unreadable_receipt)
    })?;
    rows.sort_by(|a, b| {
        a.target
            .as_str()
            .cmp(b.target.as_str())
            .then_with(|| a.receipt.skill_name.cmp(&b.receipt.skill_name))
    });
    Ok(rows)
}

pub fn scan_installed_stacks<F>(mut on_unreadable_receipt: F) -> Result<Vec<InstalledStackRow>>
where
    F: FnMut(&Path, &anyhow::Error),
{
    let mut rows = Vec::new();
    for_each_target_root(|target, root| {
        scan_target_stacks(target, root, &mut rows, &mut on_unreadable_receipt)
    })?;
    rows.sort_by(|a, b| {
        a.target
            .as_str()
            .cmp(b.target.as_str())
            .then_with(|| a.receipt.org.cmp(&b.receipt.org))
            .then_with(|| a.receipt.stack.cmp(&b.receipt.stack))
    });
    Ok(rows)
}

/// Visit the resolved root directory of every install target that exists.
/// Targets that fail to resolve or whose path does not exist are skipped;
/// a path that exists but is not a directory is an error.
fn for_each_target_root<F>(mut visit: F) -> Result<()>
where
    F: FnMut(InstallTarget, &Path) -> Result<()>,
{
    let store = ConfigStore::load().context("failed to load config")?;
    let resolver = TargetResolver::new(&store);
    for target in InstallTarget::ALL {
        let Ok(resolved) = resolver.resolve(*target) else {
            continue;
        };
        if !resolved.path.exists() {
            continue;
        }
        if !resolved.path.is_dir() {
            bail!(
                "target `{}` path `{}` exists but is not a directory",
                target.as_str(),
                resolved.path.display()
            );
        }
        visit(*target, &resolved.path)?;
    }
    Ok(())
}

fn scan_target<F>(
    target: InstallTarget,
    root: &Path,
    rows: &mut Vec<InstalledRow>,
    on_unreadable_receipt: &mut F,
) -> Result<()>
where
    F: FnMut(&Path, &anyhow::Error),
{
    let entries =
        fs::read_dir(root).with_context(|| format!("failed to read `{}`", root.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry in `{}`", root.display()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let receipt_file = path.join(RECEIPT_FILE);
        if !receipt_file.is_file() {
            continue;
        }
        match read_receipt_file(&receipt_file) {
            Ok(receipt) => rows.push(InstalledRow {
                target,
                installed_path: path,
                receipt_path: receipt_file,
                receipt,
            }),
            Err(e) => on_unreadable_receipt(&receipt_file, &e),
        }
    }
    Ok(())
}

fn scan_target_stacks<F>(
    target: InstallTarget,
    root: &Path,
    rows: &mut Vec<InstalledStackRow>,
    on_unreadable_receipt: &mut F,
) -> Result<()>
where
    F: FnMut(&Path, &anyhow::Error),
{
    let stacks_root = root.join(".agentstack-stacks");
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    if !stacks_root.exists() {
        return Ok(());
    }
    if !stacks_root.is_dir() {
        bail!(
            "stack receipt root `{}` exists but is not a directory",
            stacks_root.display()
        );
    }
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
        for stack_entry in fs::read_dir(&org_path)
            .with_context(|| format!("failed to read `{}`", org_path.display()))?
        {
            let stack_entry = stack_entry
                .with_context(|| format!("failed to read entry in `{}`", org_path.display()))?;
            let stack_path = stack_entry.path();
            ensure_stack_receipt_dir_not_symlink(&stack_path)?;
            if !stack_path.is_dir() {
                continue;
            }
            let receipt_file = stack_path.join(STACK_RECEIPT_FILE);
            if !receipt_file.is_file() {
                continue;
            }
            match read_stack_receipt_file(&receipt_file) {
                Ok(receipt) => rows.push(InstalledStackRow {
                    target,
                    target_root: root.to_path_buf(),
                    receipt_path: receipt_file,
                    receipt,
                }),
                Err(e) => on_unreadable_receipt(&receipt_file, &e),
            }
        }
    }
    Ok(())
}
