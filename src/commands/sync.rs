//! `agentstack sync` — converge install targets to a declarative repo
//! manifest of skills and stacks.
//!
//! The manifest declares what should be installed where; sync reuses the
//! existing install/update workflows to make each target match: install when
//! missing, update when outdated or drifted, and (with `--prune`) remove
//! receipt-backed installs the manifest no longer declares.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::client::configured_client;
use super::install::{
    RemoteInstallOptions, StackInstallOptions, run_remote_update_with_client,
    run_remote_with_client, run_stack_with_client,
};
use super::install_receipts::content_drift;
use super::update::{
    BatchUpdateRow, BatchUpdateRowStatus, StackUpdateOptions, StackUpdateOutcome, UpdateAllOptions,
    ensure_same_registry_url, prune_stack_items, run_all_with_client, run_stack_update_quiet,
};
use crate::config::ConfigStore;
use crate::error::CliError;
use crate::install::TargetInstallLock;
use crate::output::Ctx;
use crate::receipt::{
    InstallReceipt, RECEIPT_FILE, ReceiptSourceType, STACK_RECEIPT_FILE, StackInstallReceipt,
    StackInstallReceiptItem, ensure_stack_receipt_dir_not_symlink, read_receipt_file,
    read_receipt_from_dir, read_stack_receipt_file, receipt_path, stack_receipt_path,
    stack_referrers, validate_stack_receipt_item_paths,
};
use crate::registry::RegistryClient;
use crate::skill::check_slug;
use crate::skill_ref::SkillRef;
use crate::targets::{InstallTarget, TargetResolver};

/// Copy-pasteable manifest skeleton quoted by the missing-manifest error and
/// the command help.
pub const SYNC_MANIFEST_EXAMPLE: &str = "[[stacks]]\n\
ref = \"acme/engineering-default\"\n\
target = \"claude-code-repo\"\n\n\
[[skills]]\n\
ref = \"acme/code-review\"\n\
target = \"codex-repo\"";

pub struct Args {
    pub manifest: PathBuf,
    pub check: bool,
    pub prune: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncEntryKind {
    Skill,
    Stack,
}

impl SyncEntryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            SyncEntryKind::Skill => "skill",
            SyncEntryKind::Stack => "stack",
        }
    }
}

/// One declared manifest row, parsed and validated.
#[derive(Debug, Clone)]
pub struct SyncEntry {
    pub kind: SyncEntryKind,
    pub org: String,
    pub name: String,
    /// Skill entries only: pin to one uploaded version.
    pub pin: Option<String>,
    pub target: InstallTarget,
}

impl SyncEntry {
    pub fn display_ref(&self) -> String {
        match &self.pin {
            Some(pin) => format!("{}/{}@{pin}", self.org, self.name),
            None => format!("{}/{}", self.org, self.name),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncManifest {
    pub entries: Vec<SyncEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    #[serde(default)]
    stacks: Vec<ManifestEntry>,
    #[serde(default)]
    skills: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    #[serde(rename = "ref")]
    entry_ref: String,
    target: String,
}

pub fn load_manifest(path: &Path) -> Result<SyncManifest> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::new(
                "manifest_missing",
                format!(
                    "manifest `{}` not found; create one like:\n\n{SYNC_MANIFEST_EXAMPLE}",
                    path.display()
                ),
            )
            .resource(path.display().to_string())
            .action("sync")
            .into());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read `{}`", path.display()));
        }
    };
    parse_manifest(&text, path)
}

pub fn parse_manifest(text: &str, path: &Path) -> Result<SyncManifest> {
    let file: ManifestFile = toml::from_str(text)
        .with_context(|| format!("failed to parse manifest `{}`", path.display()))?;
    let mut entries = Vec::new();
    for raw in &file.stacks {
        entries.push(parse_stack_entry(raw)?);
    }
    for raw in &file.skills {
        entries.push(parse_skill_entry(raw)?);
    }
    if entries.is_empty() {
        bail!(
            "manifest `{}` declares no skills or stacks; add [[skills]] or [[stacks]] entries",
            path.display()
        );
    }
    let mut seen = BTreeSet::new();
    for entry in &entries {
        let key = (
            entry.kind.as_str(),
            entry.org.clone(),
            entry.name.clone(),
            entry.target.as_str(),
        );
        if !seen.insert(key) {
            bail!(
                "manifest declares {} `{}/{}` more than once for target `{}`",
                entry.kind.as_str(),
                entry.org,
                entry.name,
                entry.target.as_str()
            );
        }
    }
    Ok(SyncManifest { entries })
}

fn parse_skill_entry(raw: &ManifestEntry) -> Result<SyncEntry> {
    if !raw.entry_ref.contains('/') {
        bail!(
            "manifest skill ref `{}` must be fully qualified as `org/skill[@version]`",
            raw.entry_ref
        );
    }
    let skill_ref = SkillRef::parse(&raw.entry_ref)?;
    Ok(SyncEntry {
        kind: SyncEntryKind::Skill,
        org: skill_ref.org,
        name: skill_ref.name,
        pin: skill_ref.version,
        target: InstallTarget::parse(&raw.target)?,
    })
}

fn parse_stack_entry(raw: &ManifestEntry) -> Result<SyncEntry> {
    if raw.entry_ref.contains('@') {
        bail!(
            "manifest stack ref `{}` cannot pin a version; stacks resolve their own skill versions",
            raw.entry_ref
        );
    }
    let Some((org, stack)) = raw.entry_ref.split_once('/') else {
        bail!(
            "manifest stack ref `{}` must be fully qualified as `org/stack`",
            raw.entry_ref
        );
    };
    check_slug(org).map_err(|reason| {
        anyhow!(
            "invalid org in manifest stack ref `{}`: {reason}",
            raw.entry_ref
        )
    })?;
    check_slug(stack).map_err(|reason| {
        anyhow!(
            "invalid stack in manifest stack ref `{}`: {reason}",
            raw.entry_ref
        )
    })?;
    Ok(SyncEntry {
        kind: SyncEntryKind::Stack,
        org: org.to_string(),
        name: stack.to_string(),
        pin: None,
        target: InstallTarget::parse(&raw.target)?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAction {
    Installed,
    Updated,
    UpToDate,
    WouldInstall,
    WouldUpdate,
    Pruned,
    WouldPrune,
    Failed,
}

impl SyncAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            SyncAction::Installed => "installed",
            SyncAction::Updated => "updated",
            SyncAction::UpToDate => "up-to-date",
            SyncAction::WouldInstall => "would-install",
            SyncAction::WouldUpdate => "would-update",
            SyncAction::Pruned => "pruned",
            SyncAction::WouldPrune => "would-prune",
            SyncAction::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncEntryOutcome {
    pub kind: SyncEntryKind,
    pub entry_ref: String,
    pub target: InstallTarget,
    pub action: SyncAction,
    pub version: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub check: bool,
    pub prune: bool,
    /// Why the prune pass did not run despite `--prune` being set.
    pub prune_skipped: Option<String>,
    pub entries: Vec<SyncEntryOutcome>,
}

impl SyncOutcome {
    pub fn count(&self, action: SyncAction) -> usize {
        self.entries.iter().filter(|e| e.action == action).count()
    }

    /// Entries that would change something when applied.
    pub fn pending_count(&self) -> usize {
        self.count(SyncAction::WouldInstall)
            + self.count(SyncAction::WouldUpdate)
            + self.count(SyncAction::WouldPrune)
    }

    pub fn failed_count(&self) -> usize {
        self.count(SyncAction::Failed)
    }
}

pub struct SyncOptions<'a> {
    pub manifest: &'a SyncManifest,
    /// Resolved root path for every target the manifest mentions.
    pub target_roots: &'a [(InstallTarget, PathBuf)],
    pub check: bool,
    pub prune: bool,
    pub registry_url: Option<&'a str>,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if args.check && args.yes {
        bail!("cannot combine `--check` and `--yes`");
    }
    let manifest = load_manifest(&args.manifest)?;

    let store = ConfigStore::load().context("failed to load config")?;
    let resolver = TargetResolver::new(&store);
    let mut target_roots: Vec<(InstallTarget, PathBuf)> = Vec::new();
    for entry in &manifest.entries {
        if target_roots.iter().any(|(t, _)| *t == entry.target) {
            continue;
        }
        let resolved = resolver.resolve(entry.target)?;
        target_roots.push((entry.target, resolved.path));
    }

    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let installed_by = configured.client.whoami().ok().map(|reply| reply.user);

    let options = |check: bool| SyncOptions {
        manifest: &manifest,
        target_roots: &target_roots,
        check,
        prune: args.prune,
        registry_url: Some(&configured.url),
        installed_by: installed_by.clone(),
        cache_root: None,
    };

    if args.check {
        let outcome = run_with_client(&configured.client, options(true))?;
        render(ctx, &args.manifest, &outcome)?;
        return finish(&outcome);
    }

    if !args.yes {
        // Plan first so the confirmation names what would change; skip the
        // prompt entirely when everything is already converged.
        let plan = run_with_client(&configured.client, options(true))?;
        if plan.pending_count() == 0 {
            render(ctx, &args.manifest, &plan)?;
            return finish(&plan);
        }
        let confirmed = ctx.prompt_confirm(
            format!("sync will {}; proceed?", pending_summary(&plan)),
            "sync cannot prompt in this context; rerun with `--yes`, or use `--check` to review pending changes",
        )?;
        if !confirmed {
            ctx.say("no changes made");
            return Ok(());
        }
    }

    let outcome = run_with_client(&configured.client, options(false))?;
    render(ctx, &args.manifest, &outcome)?;
    finish(&outcome)
}

fn finish(outcome: &SyncOutcome) -> Result<()> {
    let failed = outcome.failed_count();
    if failed > 0 {
        bail!(
            "sync failed for {failed} entr{}",
            if failed == 1 { "y" } else { "ies" }
        );
    }
    Ok(())
}

fn pending_summary(outcome: &SyncOutcome) -> String {
    let mut parts = Vec::new();
    let install = outcome.count(SyncAction::WouldInstall);
    let update = outcome.count(SyncAction::WouldUpdate);
    let prune = outcome.count(SyncAction::WouldPrune);
    if install > 0 {
        parts.push(format!("install {install}"));
    }
    if update > 0 {
        parts.push(format!("update {update}"));
    }
    if prune > 0 {
        parts.push(format!("prune {prune}"));
    }
    parts.join(", ")
}

pub fn run_with_client(client: &dyn RegistryClient, opts: SyncOptions<'_>) -> Result<SyncOutcome> {
    let mut entries = Vec::new();
    for entry in &opts.manifest.entries {
        let root = root_for(opts.target_roots, entry.target)?;
        let outcome = match entry.kind {
            SyncEntryKind::Skill => sync_skill_entry(client, &opts, entry, root),
            SyncEntryKind::Stack => sync_stack_entry(client, &opts, entry, root),
        };
        entries.push(outcome.unwrap_or_else(|err| {
            entry_outcome(entry, SyncAction::Failed, None, Some(format!("{err:#}")))
        }));
    }
    let mut prune_skipped = None;
    if opts.prune {
        let failed = entries
            .iter()
            .filter(|entry| entry.action == SyncAction::Failed)
            .count();
        if failed > 0 {
            // A failed entry may still own installs the manifest declares;
            // pruning around it could delete what the entry failed to adopt.
            prune_skipped = Some(format!(
                "prune skipped: {failed} entr{} failed",
                if failed == 1 { "y" } else { "ies" }
            ));
        } else {
            entries.extend(prune_undeclared(&opts)?);
        }
    }
    Ok(SyncOutcome {
        check: opts.check,
        prune: opts.prune,
        prune_skipped,
        entries,
    })
}

fn root_for(roots: &[(InstallTarget, PathBuf)], target: InstallTarget) -> Result<&Path> {
    roots
        .iter()
        .find(|(t, _)| *t == target)
        .map(|(_, path)| path.as_path())
        .with_context(|| format!("no resolved path for target `{}`", target.as_str()))
}

fn entry_outcome(
    entry: &SyncEntry,
    action: SyncAction,
    version: Option<String>,
    detail: Option<String>,
) -> SyncEntryOutcome {
    SyncEntryOutcome {
        kind: entry.kind,
        entry_ref: entry.display_ref(),
        target: entry.target,
        action,
        version,
        detail,
    }
}

fn sync_skill_entry(
    client: &dyn RegistryClient,
    opts: &SyncOptions<'_>,
    entry: &SyncEntry,
    root: &Path,
) -> Result<SyncEntryOutcome> {
    let installed_path = root.join(&entry.name);
    let receipt_file = receipt_path(&installed_path);
    if !receipt_file.is_file() {
        if installed_path.exists() {
            bail!(
                "`{}` exists but has no AgentStack install receipt; remove it or run `agentstack skill install {} --target {} --force`",
                installed_path.display(),
                entry.display_ref(),
                entry.target.as_str()
            );
        }
        if opts.check {
            return Ok(entry_outcome(
                entry,
                SyncAction::WouldInstall,
                entry.pin.clone(),
                None,
            ));
        }
        let report = run_remote_with_client(
            client,
            RemoteInstallOptions {
                skill_ref: &entry_skill_ref(entry)?,
                dest_root: root,
                target: entry.target.as_str(),
                force: false,
                registry_url: opts.registry_url,
                installed_by: opts.installed_by.clone(),
                cache_root: opts.cache_root,
                allow_yanked: false,
            },
        )?;
        return Ok(entry_outcome(
            entry,
            SyncAction::Installed,
            Some(report.metadata.version),
            None,
        ));
    }

    let receipt = read_receipt_from_dir(&installed_path)?;
    let referrers = stack_referrers(&receipt);
    if !referrers.is_empty() {
        let stacks = referrers
            .iter()
            .map(|via| format!("`{}/{}`", via.org, via.stack))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "`{}` in target `{}` is owned by stack(s) {stacks}; declare the stack in the manifest instead",
            entry.name,
            entry.target.as_str()
        );
    }
    if receipt.source_type != ReceiptSourceType::Registry {
        bail!(
            "`{}` in target `{}` was installed from a local path (`{}`); run `agentstack skill install {} --target {} --force` to adopt it",
            entry.name,
            entry.target.as_str(),
            receipt.source_ref,
            entry.display_ref(),
            entry.target.as_str()
        );
    }
    if receipt.org.as_deref() != Some(entry.org.as_str()) {
        bail!(
            "`{}` in target `{}` is installed from `{}`, not `{}`; uninstall it first",
            entry.name,
            entry.target.as_str(),
            receipt.source_ref,
            entry.display_ref()
        );
    }

    match &entry.pin {
        Some(pin) => sync_pinned_skill(client, opts, entry, root, &installed_path, receipt, pin),
        None => sync_current_skill(
            client,
            opts,
            entry,
            root,
            installed_path,
            receipt_file,
            receipt,
        ),
    }
}

fn entry_skill_ref(entry: &SyncEntry) -> Result<SkillRef> {
    let skill_ref = SkillRef::new(entry.org.clone(), entry.name.clone())?;
    Ok(match &entry.pin {
        Some(pin) => skill_ref.with_version(pin.clone())?,
        None => skill_ref,
    })
}

fn sync_pinned_skill(
    client: &dyn RegistryClient,
    opts: &SyncOptions<'_>,
    entry: &SyncEntry,
    root: &Path,
    installed_path: &Path,
    receipt: InstallReceipt,
    pin: &str,
) -> Result<SyncEntryOutcome> {
    ensure_same_registry_url(
        receipt.registry_url.as_deref(),
        opts.registry_url,
        false,
        &format!("`{}`", entry.name),
    )?;
    let drift = content_drift(installed_path, &receipt);
    let on_pin = receipt.version.as_deref() == Some(pin);
    if on_pin && !drift.is_drifted() {
        return Ok(entry_outcome(
            entry,
            SyncAction::UpToDate,
            Some(pin.to_string()),
            None,
        ));
    }
    let detail = if on_pin {
        "restores local modifications".to_string()
    } else {
        format!(
            "{} -> {pin}",
            receipt.version.as_deref().unwrap_or("<unknown>")
        )
    };
    if opts.check {
        return Ok(entry_outcome(
            entry,
            SyncAction::WouldUpdate,
            Some(pin.to_string()),
            Some(detail),
        ));
    }
    let report = run_remote_update_with_client(
        client,
        RemoteInstallOptions {
            skill_ref: &entry_skill_ref(entry)?,
            dest_root: root,
            target: entry.target.as_str(),
            force: false,
            registry_url: opts.registry_url,
            installed_by: opts.installed_by.clone(),
            cache_root: opts.cache_root,
            allow_yanked: false,
        },
    )?;
    Ok(entry_outcome(
        entry,
        SyncAction::Updated,
        Some(report.metadata.version),
        Some(detail),
    ))
}

fn sync_current_skill(
    client: &dyn RegistryClient,
    opts: &SyncOptions<'_>,
    entry: &SyncEntry,
    root: &Path,
    installed_path: PathBuf,
    receipt_file: PathBuf,
    receipt: InstallReceipt,
) -> Result<SyncEntryOutcome> {
    // The drift-restore pass below runs the batch updater with `force`, which
    // would also waive its cross-registry provenance guard; enforce the guard
    // here first so sync never silently re-sources an install.
    ensure_same_registry_url(
        receipt.registry_url.as_deref(),
        opts.registry_url,
        false,
        &format!("`{}`", entry.name),
    )?;
    let drift = content_drift(&installed_path, &receipt);
    let row = BatchUpdateRow {
        target: entry.target,
        target_root: root.to_path_buf(),
        installed_path,
        receipt_path: receipt_file,
        skill_name: entry.name.clone(),
        receipt,
    };
    // A drifted-but-current install converges by reinstalling the recorded
    // version, which the batch updater only does under `force`.
    let force = drift.is_drifted() && !opts.check;
    let batch = run_all_with_client(
        client,
        UpdateAllOptions {
            rows: vec![row],
            target_filter: None,
            registry_url: opts.registry_url,
            check: opts.check,
            force,
            installed_by: opts.installed_by.clone(),
            cache_root: opts.cache_root,
        },
    );
    let result = batch
        .results
        .into_iter()
        .next()
        .context("sync produced no update result")?;
    Ok(match result.status {
        BatchUpdateRowStatus::AlreadyCurrent { version } => {
            if drift.is_drifted() {
                // Only reachable in check mode; an apply pass reinstalls.
                entry_outcome(
                    entry,
                    SyncAction::WouldUpdate,
                    Some(version),
                    Some("restores local modifications".to_string()),
                )
            } else {
                entry_outcome(entry, SyncAction::UpToDate, Some(version), None)
            }
        }
        BatchUpdateRowStatus::UpdateAvailable {
            installed_version,
            latest_version,
            ..
        } => {
            let detail = format!(
                "{} -> {latest_version}",
                installed_version.as_deref().unwrap_or("<unknown>")
            );
            entry_outcome(
                entry,
                SyncAction::WouldUpdate,
                Some(latest_version),
                Some(detail),
            )
        }
        BatchUpdateRowStatus::Updated {
            installed_version,
            latest_version,
            forced,
            ..
        } => {
            let detail = if forced {
                "reinstalled".to_string()
            } else {
                format!(
                    "{} -> {latest_version}",
                    installed_version.as_deref().unwrap_or("<unknown>")
                )
            };
            entry_outcome(
                entry,
                SyncAction::Updated,
                Some(latest_version),
                Some(detail),
            )
        }
        BatchUpdateRowStatus::Failed { reason } | BatchUpdateRowStatus::Skipped { reason } => {
            entry_outcome(entry, SyncAction::Failed, None, Some(reason))
        }
    })
}

fn sync_stack_entry(
    client: &dyn RegistryClient,
    opts: &SyncOptions<'_>,
    entry: &SyncEntry,
    root: &Path,
) -> Result<SyncEntryOutcome> {
    let stacks_root = root.join(".agentstack-stacks");
    let org_path = stacks_root.join(&entry.org);
    let stack_path = org_path.join(&entry.name);
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    ensure_stack_receipt_dir_not_symlink(&org_path)?;
    ensure_stack_receipt_dir_not_symlink(&stack_path)?;
    let receipt_file = stack_receipt_path(root, &entry.org, &entry.name);
    if !receipt_file.is_file() {
        if opts.check {
            return Ok(entry_outcome(entry, SyncAction::WouldInstall, None, None));
        }
        let report = run_stack_with_client(
            client,
            StackInstallOptions {
                org: &entry.org,
                stack: &entry.name,
                dest_root: root,
                target: entry.target.as_str(),
                force: false,
                registry_url: opts.registry_url,
                installed_by: opts.installed_by.clone(),
                cache_root: opts.cache_root,
            },
        )?;
        return Ok(entry_outcome(
            entry,
            SyncAction::Installed,
            None,
            Some(format!("{} skill(s)", report.installed.len())),
        ));
    }

    let stack_ref = format!("{}/{}", entry.org, entry.name);
    let outcome = run_stack_update_quiet(
        client,
        &StackUpdateOptions {
            stack: &stack_ref,
            target: entry.target,
            target_root: root,
            registry_url: opts.registry_url,
            check: opts.check,
            force: false,
            prune: opts.prune,
            json: false,
            quiet: true,
            installed_by: opts.installed_by.clone(),
            cache_root: opts.cache_root,
        },
    )?;
    let detail = stack_changes_detail(&outcome);
    if opts.check {
        if detail.is_some() {
            return Ok(entry_outcome(entry, SyncAction::WouldUpdate, None, detail));
        }
        return Ok(entry_outcome(entry, SyncAction::UpToDate, None, None));
    }
    if outcome.updated {
        return Ok(entry_outcome(entry, SyncAction::Updated, None, detail));
    }
    Ok(entry_outcome(entry, SyncAction::UpToDate, None, None))
}

fn stack_changes_detail(outcome: &StackUpdateOutcome) -> Option<String> {
    let mut parts = Vec::new();
    if !outcome.added.is_empty() {
        parts.push(format!("{} added", outcome.added.len()));
    }
    if !outcome.changed.is_empty() {
        parts.push(format!("{} updated", outcome.changed.len()));
    }
    if !outcome.removed.is_empty() {
        parts.push(format!("{} removed", outcome.removed.len()));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn prune_undeclared(opts: &SyncOptions<'_>) -> Result<Vec<SyncEntryOutcome>> {
    let mut declared_skills: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut declared_stacks: BTreeSet<(&str, String)> = BTreeSet::new();
    for entry in &opts.manifest.entries {
        match entry.kind {
            SyncEntryKind::Skill => {
                declared_skills.insert((entry.target.as_str(), entry.name.as_str()));
            }
            SyncEntryKind::Stack => {
                declared_stacks.insert((
                    entry.target.as_str(),
                    format!("{}/{}", entry.org, entry.name),
                ));
            }
        }
    }

    let mut rows = Vec::new();
    for (target, root) in opts.target_roots {
        if !root.is_dir() {
            continue;
        }
        let _lock = if opts.check {
            None
        } else {
            Some(TargetInstallLock::acquire_for_target(
                root,
                Some("sync"),
                Some(target.as_str()),
            )?)
        };

        // Stacks first: pruning a stack removes its solely-owned children, so
        // the direct-skill pass below never sees them.
        for (receipt_dir, receipt) in scan_stack_receipts(root, *target, &mut rows)? {
            let stack_ref = format!("{}/{}", receipt.org, receipt.stack);
            if declared_stacks.contains(&(target.as_str(), stack_ref.clone())) {
                continue;
            }
            if opts.check {
                rows.push(prune_outcome(
                    SyncEntryKind::Stack,
                    stack_ref,
                    *target,
                    true,
                    None,
                ));
                continue;
            }
            match prune_stack_install(root, &receipt, &receipt_dir) {
                Ok(()) => rows.push(prune_outcome(
                    SyncEntryKind::Stack,
                    stack_ref,
                    *target,
                    false,
                    None,
                )),
                Err(err) => rows.push(SyncEntryOutcome {
                    kind: SyncEntryKind::Stack,
                    entry_ref: stack_ref,
                    target: *target,
                    action: SyncAction::Failed,
                    version: None,
                    detail: Some(format!("{err:#}")),
                }),
            }
        }

        for (installed_path, receipt) in scan_skill_receipts(root, *target, &mut rows)? {
            if !stack_referrers(&receipt).is_empty() {
                // Stack-owned children are governed by their stack entry.
                continue;
            }
            let name = installed_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(receipt.skill_name.as_str())
                .to_string();
            if declared_skills.contains(&(target.as_str(), name.as_str())) {
                continue;
            }
            let display = receipt
                .org
                .as_deref()
                .map(|org| format!("{org}/{}", receipt.skill_name))
                .unwrap_or_else(|| receipt.skill_name.clone());
            if opts.check {
                rows.push(prune_outcome(
                    SyncEntryKind::Skill,
                    display,
                    *target,
                    true,
                    receipt.version.clone(),
                ));
                continue;
            }
            match remove_managed_dir(&installed_path) {
                Ok(()) => rows.push(prune_outcome(
                    SyncEntryKind::Skill,
                    display,
                    *target,
                    false,
                    receipt.version.clone(),
                )),
                Err(err) => rows.push(SyncEntryOutcome {
                    kind: SyncEntryKind::Skill,
                    entry_ref: display,
                    target: *target,
                    action: SyncAction::Failed,
                    version: receipt.version.clone(),
                    detail: Some(format!("{err:#}")),
                }),
            }
        }
    }
    Ok(rows)
}

fn prune_outcome(
    kind: SyncEntryKind,
    entry_ref: String,
    target: InstallTarget,
    check: bool,
    version: Option<String>,
) -> SyncEntryOutcome {
    SyncEntryOutcome {
        kind,
        entry_ref,
        target,
        action: if check {
            SyncAction::WouldPrune
        } else {
            SyncAction::Pruned
        },
        version,
        detail: None,
    }
}

fn scan_stack_receipts(
    root: &Path,
    target: InstallTarget,
    rows: &mut Vec<SyncEntryOutcome>,
) -> Result<Vec<(PathBuf, StackInstallReceipt)>> {
    let stacks_root = root.join(".agentstack-stacks");
    let mut found = Vec::new();
    ensure_stack_receipt_dir_not_symlink(&stacks_root)?;
    if !stacks_root.is_dir() {
        return Ok(found);
    }
    for org_entry in fs::read_dir(&stacks_root)
        .with_context(|| format!("failed to read `{}`", stacks_root.display()))?
    {
        let org_entry = org_entry
            .with_context(|| format!("failed to read entry in `{}`", stacks_root.display()))?;
        let org_path = org_entry.path();
        ensure_stack_receipt_dir_not_symlink(&org_path)?;
        if !entry_is_real_dir(&org_entry) {
            continue;
        }
        for stack_entry in fs::read_dir(&org_path)
            .with_context(|| format!("failed to read `{}`", org_path.display()))?
        {
            let stack_entry = stack_entry
                .with_context(|| format!("failed to read entry in `{}`", org_path.display()))?;
            let stack_path = stack_entry.path();
            ensure_stack_receipt_dir_not_symlink(&stack_path)?;
            if !entry_is_real_dir(&stack_entry) {
                continue;
            }
            let receipt_file = stack_path.join(STACK_RECEIPT_FILE);
            if !receipt_file.is_file() {
                continue;
            }
            match read_stack_receipt_file(&receipt_file) {
                Ok(receipt) => found.push((stack_path, receipt)),
                Err(err) => rows.push(SyncEntryOutcome {
                    kind: SyncEntryKind::Stack,
                    entry_ref: receipt_file.display().to_string(),
                    target,
                    action: SyncAction::Failed,
                    version: None,
                    detail: Some(format!("unreadable stack install receipt: {err:#}")),
                }),
            }
        }
    }
    Ok(found)
}

fn scan_skill_receipts(
    root: &Path,
    target: InstallTarget,
    rows: &mut Vec<SyncEntryOutcome>,
) -> Result<Vec<(PathBuf, InstallReceipt)>> {
    let mut found = Vec::new();
    for entry in
        fs::read_dir(root).with_context(|| format!("failed to read `{}`", root.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in `{}`", root.display()))?;
        let path = entry.path();
        if !entry_is_real_dir(&entry) {
            continue;
        }
        let receipt_file = path.join(RECEIPT_FILE);
        if !receipt_file.is_file() {
            continue;
        }
        match read_receipt_file(&receipt_file) {
            Ok(receipt) => found.push((path, receipt)),
            Err(err) => rows.push(SyncEntryOutcome {
                kind: SyncEntryKind::Skill,
                entry_ref: receipt_file.display().to_string(),
                target,
                action: SyncAction::Failed,
                version: None,
                detail: Some(format!("unreadable install receipt: {err:#}")),
            }),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn prune_stack_install(
    root: &Path,
    receipt: &StackInstallReceipt,
    receipt_dir: &Path,
) -> Result<()> {
    for item in &receipt.items {
        validate_stack_receipt_item_paths(root, item)?;
        ensure_stack_child_prunable(receipt, item)?;
    }
    prune_stack_items(receipt, &receipt.items)?;
    remove_managed_dir(receipt_dir)?;
    if let Some(org_dir) = receipt_dir.parent() {
        // Best effort: only succeeds when the org directory is now empty.
        let _ = fs::remove_dir(org_dir);
    }
    Ok(())
}

fn ensure_stack_child_prunable(
    receipt: &StackInstallReceipt,
    item: &StackInstallReceiptItem,
) -> Result<()> {
    let child = match read_receipt_from_dir(&item.install_path) {
        Ok(child) => child,
        Err(_) => {
            if !item.install_path.exists() {
                return Ok(());
            }
            bail!(
                "refusing to prune stack child `{}`: `{}` has no readable install receipt; run `agentstack stack uninstall {}/{} --target {} --force`",
                item.skill,
                item.install_path.display(),
                receipt.org,
                receipt.stack,
                receipt.target
            );
        }
    };
    if stack_referrers(&child)
        .iter()
        .any(|via| via.org == receipt.org && via.stack == receipt.stack)
    {
        return Ok(());
    }
    bail!(
        "refusing to prune stack child `{}` because it is not owned by stack `{}/{}`",
        item.skill,
        receipt.org,
        receipt.stack
    );
}

/// No-follow directory check used by the skill receipt scanner.
fn entry_is_real_dir(entry: &fs::DirEntry) -> bool {
    entry
        .file_type()
        .map(|kind| kind.is_dir() && !kind.is_symlink())
        .unwrap_or(false)
}

fn remove_managed_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat `{}`", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("`{}` is a symlink; refusing to remove it", path.display());
    }
    if !metadata.is_dir() {
        bail!(
            "`{}` is not a directory; refusing to remove it",
            path.display()
        );
    }
    fs::remove_dir_all(path).with_context(|| format!("failed to remove `{}`", path.display()))
}

#[derive(Serialize)]
struct SyncJson<'a> {
    kind: &'static str,
    manifest: &'a Path,
    check: bool,
    prune: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prune_skipped: Option<&'a str>,
    entries: Vec<SyncEntryJson<'a>>,
    summary: SyncSummaryJson,
}

#[derive(Serialize)]
struct SyncEntryJson<'a> {
    #[serde(rename = "ref")]
    entry_ref: &'a str,
    target: &'static str,
    kind: &'static str,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
}

#[derive(Serialize)]
struct SyncSummaryJson {
    installed: usize,
    updated: usize,
    up_to_date: usize,
    would_install: usize,
    would_update: usize,
    pruned: usize,
    would_prune: usize,
    failed: usize,
}

fn render(ctx: &Ctx, manifest_path: &Path, outcome: &SyncOutcome) -> Result<()> {
    if ctx.json {
        let payload = SyncJson {
            kind: "sync",
            manifest: manifest_path,
            check: outcome.check,
            prune: outcome.prune,
            prune_skipped: outcome.prune_skipped.as_deref(),
            entries: outcome
                .entries
                .iter()
                .map(|entry| SyncEntryJson {
                    entry_ref: &entry.entry_ref,
                    target: entry.target.as_str(),
                    kind: entry.kind.as_str(),
                    action: entry.action.as_str(),
                    version: entry.version.as_deref(),
                    detail: entry.detail.as_deref(),
                })
                .collect(),
            summary: SyncSummaryJson {
                installed: outcome.count(SyncAction::Installed),
                updated: outcome.count(SyncAction::Updated),
                up_to_date: outcome.count(SyncAction::UpToDate),
                would_install: outcome.count(SyncAction::WouldInstall),
                would_update: outcome.count(SyncAction::WouldUpdate),
                pruned: outcome.count(SyncAction::Pruned),
                would_prune: outcome.count(SyncAction::WouldPrune),
                failed: outcome.failed_count(),
            },
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let ref_w = outcome
        .entries
        .iter()
        .map(|e| e.entry_ref.len())
        .max()
        .unwrap_or(0);
    let target_w = outcome
        .entries
        .iter()
        .map(|e| e.target.as_str().len())
        .max()
        .unwrap_or(0);
    for entry in &outcome.entries {
        let detail = entry
            .detail
            .clone()
            .or_else(|| entry.version.as_ref().map(|v| format!("v{v}")))
            .unwrap_or_default();
        ctx.say(format!(
            "{entry_ref:<ref_w$}  {target:<target_w$}  {kind:<5}  {action:<13}  {detail}",
            entry_ref = entry.entry_ref,
            target = entry.target.as_str(),
            kind = entry.kind.as_str(),
            action = entry.action.as_str(),
        ));
    }
    if let Some(reason) = &outcome.prune_skipped {
        ctx.say(format!("note: {reason}"));
    }
    ctx.say(summary_line(outcome));
    Ok(())
}

fn summary_line(outcome: &SyncOutcome) -> String {
    if outcome.check {
        format!(
            "summary: would-install {} | would-update {} | up-to-date {} | would-prune {} | failed {}",
            outcome.count(SyncAction::WouldInstall),
            outcome.count(SyncAction::WouldUpdate),
            outcome.count(SyncAction::UpToDate),
            outcome.count(SyncAction::WouldPrune),
            outcome.failed_count(),
        )
    } else {
        format!(
            "summary: installed {} | updated {} | up-to-date {} | pruned {} | failed {}",
            outcome.count(SyncAction::Installed),
            outcome.count(SyncAction::Updated),
            outcome.count(SyncAction::UpToDate),
            outcome.count(SyncAction::Pruned),
            outcome.failed_count(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<SyncManifest> {
        parse_manifest(text, Path::new("agentstack.toml"))
    }

    #[test]
    fn manifest_parses_stacks_and_pinned_skills() {
        let manifest = parse(
            "[[stacks]]\nref = \"acme/engineering-default\"\ntarget = \"local\"\n\n\
             [[skills]]\nref = \"acme/code-review@2\"\ntarget = \"local\"\n",
        )
        .unwrap();
        assert_eq!(manifest.entries.len(), 2);
        let stack = &manifest.entries[0];
        assert_eq!(stack.kind, SyncEntryKind::Stack);
        assert_eq!(stack.display_ref(), "acme/engineering-default");
        let skill = &manifest.entries[1];
        assert_eq!(skill.kind, SyncEntryKind::Skill);
        assert_eq!(skill.pin.as_deref(), Some("2"));
        assert_eq!(skill.display_ref(), "acme/code-review@2");
        assert_eq!(skill.target, InstallTarget::Local);
    }

    #[test]
    fn manifest_rejects_bare_refs_and_stack_pins() {
        let err = parse("[[skills]]\nref = \"code-review\"\ntarget = \"local\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("fully qualified"), "{err:#}");

        let err = parse("[[stacks]]\nref = \"acme/base@2\"\ntarget = \"local\"\n").unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot pin a version"),
            "{err:#}"
        );

        let err = parse("[[stacks]]\nref = \"base\"\ntarget = \"local\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("`org/stack`"), "{err:#}");
    }

    #[test]
    fn manifest_rejects_duplicates_unknown_targets_and_unknown_keys() {
        let err = parse(
            "[[skills]]\nref = \"acme/a\"\ntarget = \"local\"\n\n\
             [[skills]]\nref = \"acme/a\"\ntarget = \"local\"\n",
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("more than once"), "{err:#}");

        let err = parse("[[skills]]\nref = \"acme/a\"\ntarget = \"nope\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");

        let err =
            parse("[[skills]]\nref = \"acme/a\"\ntarget = \"local\"\npin = \"2\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("pin"), "{err:#}");
    }

    #[test]
    fn missing_manifest_error_shows_the_format() {
        let err = load_manifest(Path::new("/nonexistent/agentstack.toml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("[[stacks]]"), "{msg}");
        assert!(msg.contains("[[skills]]"), "{msg}");
        let cli = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(cli.code, "manifest_missing");
    }

    #[test]
    fn empty_manifest_is_rejected() {
        let err = parse("").unwrap_err();
        assert!(
            format!("{err:#}").contains("declares no skills or stacks"),
            "{err:#}"
        );
    }
}
