//! Installer service.
//!
//! Copies a validated skill directory into a target root under its skill
//! name. The installer:
//!
//! 1. Runs hard validation against `source` (refuses to copy anything that
//!    isn't a real skill).
//! 2. Runs soft lint to surface warnings on the install report — they don't
//!    block installation, but they're shown to the user.
//! 3. Computes the destination as `dest_root/<skill-name>`.
//! 4. Refuses to overwrite an existing destination unless `force` is set.
//! 5. Copies regular files, applying the same exclusion rules as `pack`
//!    (no `.git`, `.DS_Store`, secrets, etc.).
//! 6. When the install target maps to a platform (e.g. `claude-code`),
//!    overlays `platform/<name>/` files over the staged copy before the
//!    content hash is recorded.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::CliError;
use crate::package::{
    HASH_ALGORITHM, PackageHash, ensure_safe_path, hex_digest, is_excluded_dir, is_excluded_file,
};
use crate::receipt::{
    InstallReceipt, InstallReceiptRequest, ReceiptSourceType, read_receipt_from_dir, receipt_path,
    stack_referrers, write_receipt_to_dir,
};
use crate::skill::{
    DEFAULT_SOFT_CHAR_LIMIT, LintConfig, ValidationOutcome, lint_skill, validate_skill,
};
use crate::targets::InstallTarget;
use sha2::{Digest, Sha256};

const TARGET_LOCK_DIR: &str = ".agentstack-install.lock";
const TARGET_LOCK_METADATA: &str = "lock.json";
const TARGET_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const TARGET_LOCK_POLL: Duration = Duration::from_millis(25);
const STALE_LOCK_THRESHOLD: Duration = Duration::from_secs(30 * 60);

pub const TARGET_INSTALL_LOCK_DIR: &str = TARGET_LOCK_DIR;
pub const TARGET_INSTALL_LOCK_METADATA: &str = TARGET_LOCK_METADATA;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetLockMetadata {
    pub pid: u32,
    pub hostname: Option<String>,
    pub created_at: String,
    pub command_kind: Option<String>,
    pub target_root: PathBuf,
    pub process: TargetLockProcessInfo,
    pub agentstack: TargetLockAgentStackInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetLockProcessInfo {
    pub executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetLockAgentStackInfo {
    pub version: String,
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetLockDiagnostics {
    pub target_root: PathBuf,
    pub lock_path: PathBuf,
    pub exists: bool,
    pub metadata_path: PathBuf,
    pub metadata: Option<TargetLockMetadata>,
    pub metadata_error: Option<String>,
    pub age_seconds: Option<u64>,
    pub stale: bool,
    pub stale_after_seconds: u64,
}

#[derive(Debug)]
pub struct TargetBusyError {
    pub target_root: PathBuf,
    pub lock_path: PathBuf,
    pub lock_age: Option<Duration>,
    pub pid: Option<u32>,
    pub hostname: Option<String>,
    pub suggested_next_command: String,
}

impl TargetBusyError {
    pub fn age_label(&self) -> String {
        self.lock_age
            .map(format_duration)
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl std::fmt::Display for TargetBusyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "target_busy: install target `{}` is locked by another AgentStack install/update (lock: `{}`, age: {}); retry after it completes or run `{}` to inspect the lock",
            self.target_root.display(),
            self.lock_path.display(),
            self.age_label(),
            self.suggested_next_command
        )
    }
}

impl std::error::Error for TargetBusyError {}

/// Cross-process guard for mutation of one install target root.
///
/// The lock is implemented as an atomically-created directory inside the
/// target root so it works with the standard library on macOS and Linux.
#[derive(Debug)]
pub struct TargetInstallLock {
    path: PathBuf,
}

impl TargetInstallLock {
    pub fn acquire_for_target(
        target_root: &Path,
        command_kind: Option<&str>,
        target_hint: Option<&str>,
    ) -> Result<Self> {
        Self::acquire_with_timeout(
            target_root,
            command_kind,
            target_hint,
            target_lock_timeout(),
        )
    }

    fn acquire_with_timeout(
        target_root: &Path,
        command_kind: Option<&str>,
        target_hint: Option<&str>,
        timeout: Duration,
    ) -> Result<Self> {
        fs::create_dir_all(target_root)
            .with_context(|| format!("failed to create `{}`", target_root.display()))?;
        let target_root = target_root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize install target `{}`",
                target_root.display()
            )
        })?;
        let path = target_root.join(TARGET_LOCK_DIR);
        let start = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let metadata = TargetLockMetadata::new(&target_root, command_kind)?;
                    write_lock_metadata(&path, &metadata)?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    if start.elapsed() >= timeout {
                        let diagnostics = diagnose_target_lock(&target_root);
                        return Err(TargetBusyError {
                            target_root,
                            lock_path: path,
                            lock_age: diagnostics.age_seconds.map(Duration::from_secs),
                            pid: diagnostics.metadata.as_ref().map(|m| m.pid),
                            hostname: diagnostics
                                .metadata
                                .as_ref()
                                .and_then(|m| m.hostname.clone()),
                            suggested_next_command: match target_hint {
                                Some(target) => {
                                    format!("agentstack install doctor --target {target}")
                                }
                                None => "agentstack install doctor --target <target>".to_string(),
                            },
                        }
                        .into());
                    }
                    thread::sleep(TARGET_LOCK_POLL);
                }
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create `{}`", path.display()));
                }
            }
        }
    }
}

impl Drop for TargetInstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl TargetLockMetadata {
    pub fn new(target_root: &Path, command_kind: Option<&str>) -> Result<Self> {
        Ok(Self {
            pid: std::process::id(),
            hostname: hostname(),
            created_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .context("failed to format lock timestamp")?,
            command_kind: command_kind.map(str::to_string),
            target_root: target_root.to_path_buf(),
            process: TargetLockProcessInfo {
                executable: std::env::current_exe().ok(),
            },
            agentstack: TargetLockAgentStackInfo {
                version: env!("CARGO_PKG_VERSION").to_string(),
                commit: option_env!("AGENTSTACK_BUILD_COMMIT").map(str::to_string),
            },
        })
    }

    fn created_at_time(&self) -> Option<SystemTime> {
        let parsed = OffsetDateTime::parse(&self.created_at, &Rfc3339).ok()?;
        Some(SystemTime::from(parsed))
    }
}

pub fn target_lock_path(target_root: &Path) -> PathBuf {
    target_root.join(TARGET_LOCK_DIR)
}

pub fn target_lock_metadata_path(target_root: &Path) -> PathBuf {
    target_lock_path(target_root).join(TARGET_LOCK_METADATA)
}

pub fn diagnose_target_lock(target_root: &Path) -> TargetLockDiagnostics {
    let lock_path = target_lock_path(target_root);
    let metadata_path = lock_path.join(TARGET_LOCK_METADATA);
    let exists = lock_path.is_dir();
    let (metadata, metadata_error) = read_target_lock_metadata(target_root)
        .map(|m| (Some(m), None))
        .unwrap_or_else(|err| {
            if exists {
                (None, Some(err.to_string()))
            } else {
                (None, None)
            }
        });
    let age_seconds = lock_age_seconds(&lock_path, metadata.as_ref());
    let stale_after_seconds = STALE_LOCK_THRESHOLD.as_secs();
    let stale = age_seconds.is_some_and(|age| age >= stale_after_seconds);

    TargetLockDiagnostics {
        target_root: target_root.to_path_buf(),
        lock_path,
        exists,
        metadata_path,
        metadata,
        metadata_error,
        age_seconds,
        stale,
        stale_after_seconds,
    }
}

pub fn read_target_lock_metadata(target_root: &Path) -> Result<TargetLockMetadata> {
    let path = target_lock_metadata_path(target_root);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read lock metadata `{}`", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse lock metadata `{}`", path.display()))
}

pub fn remove_stale_target_lock(target_root: &Path, force: bool) -> Result<TargetLockDiagnostics> {
    let diagnostics = diagnose_target_lock(target_root);
    if !diagnostics.exists {
        return Ok(diagnostics);
    }
    if !diagnostics.stale && !force {
        bail!(
            "refusing to remove fresh-looking install lock `{}` (age: {}); rerun with --force only after confirming no AgentStack install/update is active",
            diagnostics.lock_path.display(),
            diagnostics
                .age_seconds
                .map(|age| format_duration(Duration::from_secs(age)))
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    fs::remove_dir_all(&diagnostics.lock_path)
        .with_context(|| format!("failed to remove `{}`", diagnostics.lock_path.display()))?;
    Ok(diagnostics)
}

fn write_lock_metadata(lock_path: &Path, metadata: &TargetLockMetadata) -> Result<()> {
    let path = lock_path.join(TARGET_LOCK_METADATA);
    let text =
        serde_json::to_string_pretty(metadata).context("failed to serialize lock metadata")?;
    fs::write(&path, text).with_context(|| format!("failed to write `{}`", path.display()))
}

fn lock_age_seconds(lock_path: &Path, metadata: Option<&TargetLockMetadata>) -> Option<u64> {
    let created = metadata
        .and_then(TargetLockMetadata::created_at_time)
        .or_else(|| fs::metadata(lock_path).ok()?.modified().ok())?;
    SystemTime::now()
        .duration_since(created)
        .ok()
        .map(|d| d.as_secs())
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| {
                    if out.status.success() {
                        String::from_utf8(out.stdout).ok()
                    } else {
                        None
                    }
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn target_lock_timeout() -> Duration {
    std::env::var("AGENTSTACK_TARGET_LOCK_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(TARGET_LOCK_TIMEOUT)
}

/// Inputs to [`install_skill`].
pub struct InstallOptions<'a> {
    /// Path to the skill directory on disk.
    pub source: &'a Path,
    /// Directory under which the skill will be created. Created if missing.
    pub dest_root: &'a Path,
    /// Reserved for future alias support. Currently rejected to preserve
    /// the invariant that the directory name matches `SKILL.md`.
    pub name_override: Option<&'a str>,
    /// Replace an existing destination if true; refuse to overwrite if false.
    pub force: bool,
    /// Permit replacing an existing destination whose AgentStack receipt
    /// matches the incoming identity. Direct `skill install` keeps this false;
    /// `install update` enables it after deciding an update is intended.
    pub replace_matching: bool,
    /// Optional provenance receipt to write into the staged install before the
    /// final move.
    pub receipt: Option<InstallReceiptRequest>,
}

/// Outcome of a successful install.
#[derive(Debug, Clone)]
pub struct InstallReport {
    /// Skill name as written under `dest_root`.
    pub installed_as: String,
    /// Manifest name from `SKILL.md`.
    pub manifest_name: String,
    /// Concrete `dest_root/<installed_as>` path the skill was copied to.
    pub destination: PathBuf,
    /// Number of regular files copied.
    pub files_copied: usize,
    /// Whether an existing destination was replaced.
    pub overwrote_existing: bool,
    /// True when `--force` was used to replace a destination that did not match
    /// the incoming skill's identity (different org/name, missing receipt, or
    /// a local↔registry source-type swap). Callers should surface this to the
    /// user; identity-matched upgrades leave this as false.
    pub replaced_foreign: bool,
    /// Soft lint findings, surfaced for visibility but not fatal.
    pub warnings: Vec<String>,
    /// Path to the receipt written inside the installed skill, when enabled.
    pub receipt_path: Option<PathBuf>,
    /// Receipt payload written for this install, when enabled.
    pub receipt: Option<InstallReceipt>,
    /// Platform overlay applied after the base copy, when the target maps to
    /// a platform and the skill ships a matching `platform/<name>/` directory.
    pub overlay: Option<AppliedOverlay>,
}

/// A `platform/<name>/` overlay that was copied over the installed skill root.
#[derive(Debug, Clone, Serialize)]
pub struct AppliedOverlay {
    /// Platform name the overlay was taken from (e.g. `claude-code`).
    pub platform: String,
    /// Number of overlay files copied over the base install.
    pub files: usize,
}

impl AppliedOverlay {
    /// Human summary fragment, e.g. `claude-code (3 files)`.
    pub fn describe(&self) -> String {
        let noun = if self.files == 1 { "file" } else { "files" };
        format!("{} ({} {noun})", self.platform, self.files)
    }
}

/// Validate `opts.source`, then copy it under `opts.dest_root`.
pub fn install_skill(opts: InstallOptions<'_>) -> Result<InstallReport> {
    if opts.name_override.is_some() {
        bail!(
            "install aliases are not supported yet; install keeps the SKILL.md name to preserve validation and updates"
        );
    }

    let outcome = validate_skill(opts.source);
    let manifest = require_valid(&outcome, opts.source)?;

    let installed_as = manifest.name.clone();

    let warnings = collect_warnings(opts.source, &outcome);

    let dest_root = opts.dest_root;
    if dest_root.exists() && !dest_root.is_dir() {
        bail!(
            "target path `{}` exists but is not a directory",
            dest_root.display()
        );
    }
    let target_hint = opts.receipt.as_ref().map(|request| request.target.as_str());
    let _lock = TargetInstallLock::acquire_for_target(dest_root, Some("install"), target_hint)?;
    let destination = dest_root.join(&installed_as);
    let install_entries = collect_install_entries(opts.source)?;

    // The overlay platform comes from the receipt's install target; installs
    // without a receipt have no target and therefore no overlay.
    let overlay_platform = opts
        .receipt
        .as_ref()
        .and_then(|request| InstallTarget::parse(&request.target).ok())
        .and_then(InstallTarget::platform);

    let incoming_identity = opts
        .receipt
        .as_ref()
        .map(|request| (request.source_type, request.org.clone()));
    let incoming = IncomingIdentity {
        skill_name: &installed_as,
        receipt: incoming_identity
            .as_ref()
            .map(|(source_type, org)| (*source_type, org.as_deref())),
    };

    let collision =
        validate_existing_destination(&destination, opts.force, opts.replace_matching, &incoming)?;
    let staging = create_staging_dir(dest_root, &installed_as)?;
    let result = stage_install(
        opts.source,
        &staging,
        &install_entries,
        overlay_platform,
        &installed_as,
        opts.receipt,
        &destination,
    )
    .and_then(|staged| {
        commit_staged_install(
            &staging,
            &destination,
            opts.force,
            opts.replace_matching,
            collision,
            &incoming,
        )
        .map(|overwrote_existing| (staged, overwrote_existing))
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    let (staged, overwrote_existing) = result?;
    let receipt_path = staged.receipt.as_ref().map(|_| receipt_path(&destination));
    let replaced_foreign = overwrote_existing && matches!(collision, Collision::Foreign);

    Ok(InstallReport {
        installed_as,
        manifest_name: manifest.name,
        destination,
        files_copied: staged.files_copied,
        overwrote_existing,
        replaced_foreign,
        warnings,
        receipt_path,
        receipt: staged.receipt,
        overlay: staged.overlay,
    })
}

/// Files written into the staging directory, before the final move.
struct StagedInstall {
    files_copied: usize,
    overlay: Option<AppliedOverlay>,
    receipt: Option<InstallReceipt>,
}

/// Copy the skill into `staging`, apply any platform overlay, then hash the
/// staged tree and write the receipt. Hashing happens after the overlay so
/// `content_hash` reflects the files that actually land in the target.
fn stage_install(
    source: &Path,
    staging: &Path,
    entries: &[InstallEntry],
    overlay_platform: Option<&str>,
    installed_as: &str,
    receipt_request: Option<InstallReceiptRequest>,
    destination: &Path,
) -> Result<StagedInstall> {
    let files_copied = copy_skill_tree(source, staging, entries)?;
    let overlay = apply_platform_overlay(staging, overlay_platform)?;
    if let Some(overlay) = &overlay {
        require_valid_overlaid(staging, installed_as, &overlay.platform)?;
    }
    let receipt = match receipt_request {
        Some(mut request) => {
            let content_hash = hash_installable_tree_at(staging)?;
            if request.hash.is_none() {
                request.hash = Some(content_hash.clone());
            }
            if request.content_hash.is_none() {
                request.content_hash = Some(content_hash);
            }
            Some(InstallReceipt::from_request(
                installed_as.to_string(),
                destination.to_path_buf(),
                request,
            )?)
        }
        None => None,
    };
    if let Some(receipt) = &receipt {
        write_receipt_to_dir(staging, receipt)?;
    }
    Ok(StagedInstall {
        files_copied,
        overlay,
        receipt,
    })
}

/// Directory that holds per-platform adaptations inside a skill.
const PLATFORM_DIR: &str = "platform";

/// Copy `platform/<platform>/` files over the staged skill root, replacing
/// base files at the same relative paths. The `platform/` directory itself is
/// left in place. Returns `None` when there is no platform or no matching
/// overlay directory with files.
fn apply_platform_overlay(
    staging: &Path,
    platform: Option<&str>,
) -> Result<Option<AppliedOverlay>> {
    let Some(platform) = platform else {
        return Ok(None);
    };
    let overlay_root = staging.join(PLATFORM_DIR).join(platform);
    if !overlay_root.is_dir() {
        return Ok(None);
    }
    let entries = collect_install_entries(&overlay_root)?;
    let mut files = 0usize;
    for entry in &entries {
        ensure_safe_path(&entry.rel).with_context(|| {
            format!(
                "refusing platform overlay `{PLATFORM_DIR}/{platform}` entry `{}`",
                entry.rel.display()
            )
        })?;
        let target = staging.join(&entry.rel);
        match entry.kind {
            InstallEntryKind::Directory => {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create `{}`", target.display()))?;
            }
            InstallEntryKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create `{}`", parent.display()))?;
                }
                let source_path = overlay_root.join(&entry.rel);
                fs::copy(&source_path, &target).with_context(|| {
                    format!(
                        "failed to copy `{}` -> `{}`",
                        source_path.display(),
                        target.display()
                    )
                })?;
                files += 1;
            }
        }
    }
    if files == 0 {
        return Ok(None);
    }
    Ok(Some(AppliedOverlay {
        platform: platform.to_string(),
        files,
    }))
}

/// Re-validate the staged tree after an overlay was applied: an overlay must
/// not turn the skill invalid or rename it away from its install directory.
fn require_valid_overlaid(staging: &Path, installed_as: &str, platform: &str) -> Result<()> {
    let outcome = crate::skill::validate_skill_with_expected_dir_name(staging, Some(installed_as));
    if !outcome.is_ok() {
        let summary = outcome
            .errors
            .iter()
            .map(|e| format!("[{}] {}", e.code, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("platform overlay `{PLATFORM_DIR}/{platform}` produced an invalid skill: {summary}");
    }
    Ok(())
}

/// Identity of the install we're about to write, used to compare against any
/// receipt already at the destination.
struct IncomingIdentity<'a> {
    skill_name: &'a str,
    /// `None` means a local install (no receipt requested or local source).
    /// `Some((source_type, org))` carries the incoming source type and, for
    /// registry installs, the org. Org is `None` for local source type.
    receipt: Option<(ReceiptSourceType, Option<&'a str>)>,
}

#[cfg(test)]
impl<'a> IncomingIdentity<'a> {
    fn from_receipt(skill_name: &'a str, receipt: Option<&'a InstallReceipt>) -> Self {
        Self {
            skill_name,
            receipt: receipt.map(|r| (r.source_type, r.org.as_deref())),
        }
    }
}

/// Result of inspecting the destination prior to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collision {
    /// Destination did not exist (or was empty of an AgentStack receipt and we
    /// did not need to evaluate identity, e.g. on the happy first-install).
    None,
    /// Existing receipt matches incoming identity — straightforward upgrade.
    Match,
    /// Existing dir is foreign (no receipt, or receipt with a different
    /// identity). Only reachable when `--force` was set.
    Foreign,
}

fn require_valid(
    outcome: &ValidationOutcome,
    source: &Path,
) -> Result<crate::skill::SkillManifest> {
    if !outcome.is_ok() {
        let summary = outcome
            .errors
            .iter()
            .map(|e| format!("[{}] {}", e.code, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        let code = outcome
            .errors
            .first()
            .map(|error| error.code.as_str())
            .unwrap_or("validation_failed");
        let message = format!("`{}` is not a valid skill: {summary}", source.display());
        return Err(CliError::new(code, message)
            .resource(source.display().to_string())
            .action("validate_skill")
            .into());
    }
    outcome
        .manifest()
        .ok_or_else(|| anyhow!("skill validated but no manifest could be extracted"))
}

fn collect_warnings(source: &Path, outcome: &ValidationOutcome) -> Vec<String> {
    let parsed = match outcome.parsed.as_ref() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match outcome.content.as_deref() {
        Some(c) => c,
        None => return Vec::new(),
    };
    let cfg = LintConfig {
        soft_char_limit: DEFAULT_SOFT_CHAR_LIMIT,
    };
    lint_skill(source, parsed, content, &cfg)
        .into_iter()
        .map(|w| format!("[{}] {}", w.code, w.message))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallEntryKind {
    Directory,
    File,
}

#[derive(Debug, Clone)]
struct InstallEntry {
    rel: PathBuf,
    kind: InstallEntryKind,
}

fn copy_skill_tree(source: &Path, destination: &Path, entries: &[InstallEntry]) -> Result<usize> {
    let mut count = 0usize;
    for entry in entries {
        let target = destination.join(&entry.rel);
        match entry.kind {
            InstallEntryKind::Directory => {
                fs::create_dir_all(&target)
                    .with_context(|| format!("failed to create `{}`", target.display()))?;
            }
            InstallEntryKind::File => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create `{}`", parent.display()))?;
                }
                let source_path = source.join(&entry.rel);
                fs::copy(&source_path, &target).with_context(|| {
                    format!(
                        "failed to copy `{}` -> `{}`",
                        source_path.display(),
                        target.display()
                    )
                })?;
                count += 1;
            }
        }
    }
    Ok(count)
}

pub(crate) fn hash_installable_tree_at(source: &Path) -> Result<PackageHash> {
    let entries = collect_install_entries(source)?;
    hash_installable_tree(source, &entries)
}

fn hash_installable_tree(source: &Path, entries: &[InstallEntry]) -> Result<PackageHash> {
    let mut files = entries
        .iter()
        .filter(|entry| entry.kind == InstallEntryKind::File)
        .map(|entry| entry.rel.clone())
        .collect::<Vec<_>>();
    files.sort();

    let mut hasher = Sha256::new();
    for rel in files {
        let rel_path = forward_slashes(&rel);
        let path = source.join(&rel);
        let data =
            fs::read(&path).with_context(|| format!("failed to read `{}`", path.display()))?;
        hasher.update(rel_path.as_bytes());
        hasher.update([0]);
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update([0]);
        hasher.update(data);
        hasher.update([0]);
    }

    let digest = hasher.finalize();
    Ok(PackageHash {
        algorithm: HASH_ALGORITHM.to_string(),
        hex: hex_digest(&digest),
    })
}

fn collect_install_entries(source: &Path) -> Result<Vec<InstallEntry>> {
    let mut entries = Vec::new();
    collect_install_entries_recursive(source, source, &mut entries)?;
    Ok(entries)
}

fn collect_install_entries_recursive(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<InstallEntry>,
) -> Result<()> {
    let read = fs::read_dir(dir).with_context(|| format!("failed to read `{}`", dir.display()))?;
    for entry in read {
        let entry =
            entry.with_context(|| format!("failed to read entry in `{}`", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let ft = entry
            .file_type()
            .with_context(|| format!("failed to read file type for `{}`", path.display()))?;
        if ft.is_dir() {
            if is_excluded_dir(&name_str) {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            entries.push(InstallEntry {
                rel,
                kind: InstallEntryKind::Directory,
            });
            collect_install_entries_recursive(root, &path, entries)?;
        } else if ft.is_file() {
            if is_excluded_file(&name_str) {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            entries.push(InstallEntry {
                rel,
                kind: InstallEntryKind::File,
            });
        }
    }
    Ok(())
}

fn forward_slashes(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_existing_destination(
    destination: &Path,
    force: bool,
    replace_matching: bool,
    incoming: &IncomingIdentity<'_>,
) -> Result<Collision> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "`{}` is a symlink; refusing to install into it",
                    destination.display()
                );
            }
            if !metadata.is_dir() {
                bail!("`{}` exists and is not a directory", destination.display());
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Collision::None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat `{}`", destination.display()));
        }
    }

    let existing = load_existing_receipt(destination)?;
    let identity_match = existing
        .as_ref()
        .is_some_and(|receipt| identity_matches(incoming, receipt));

    if identity_match {
        if let Some(receipt) = existing.as_ref() {
            ensure_direct_update_target(destination, receipt, replace_matching)?;
        }
        if !force && !replace_matching {
            bail!(
                "refusing to replace existing install at `{}`: it already holds {}; rerun with --force to replace it or use `agentstack skill update` for registry skill updates",
                destination.display(),
                describe_incoming(incoming),
            );
        }
        return Ok(Collision::Match);
    }

    if !force {
        bail!(refusal_message(destination, existing.as_ref(), incoming));
    }
    Ok(Collision::Foreign)
}

fn ensure_direct_update_target(
    destination: &Path,
    receipt: &InstallReceipt,
    replace_matching: bool,
) -> Result<()> {
    if replace_matching && !stack_referrers(receipt).is_empty() {
        bail!(
            "refusing to update stack-managed install at `{}`; use stack update for stack-owned skills",
            destination.display()
        );
    }
    Ok(())
}

/// Read an existing receipt at `destination`. Treat a missing receipt as
/// "foreign" by returning `Ok(None)`. Surface other I/O / parse errors so the
/// caller can decide whether to bail.
pub(crate) fn load_existing_receipt(destination: &Path) -> Result<Option<InstallReceipt>> {
    let path = receipt_path(destination);
    match fs::metadata(&path) {
        Ok(_) => read_receipt_from_dir(destination).map(Some),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("failed to stat install receipt `{}`", path.display()))
        }
    }
}

fn identity_matches(incoming: &IncomingIdentity<'_>, existing: &InstallReceipt) -> bool {
    if incoming.skill_name != existing.skill_name {
        return false;
    }
    let Some((incoming_source, incoming_org)) = incoming.receipt else {
        // Incoming has no receipt at all (e.g. caller skipped provenance).
        // Refuse to claim a match in that case — we can't prove identity.
        return false;
    };
    if incoming_source != existing.source_type {
        return false;
    }
    match incoming_source {
        ReceiptSourceType::Local => true,
        ReceiptSourceType::Registry => incoming_org == existing.org.as_deref(),
    }
}

fn refusal_message(
    destination: &Path,
    existing: Option<&InstallReceipt>,
    incoming: &IncomingIdentity<'_>,
) -> String {
    let installed = existing
        .map(describe_existing)
        .unwrap_or_else(|| "an unmanaged directory (no AgentStack install receipt)".to_string());
    let arriving = describe_incoming(incoming);
    format!(
        "refusing to overwrite `{}`: it currently holds {installed}, but you are installing {arriving}; rerun with --force to replace it",
        destination.display()
    )
}

pub(crate) fn describe_existing(receipt: &InstallReceipt) -> String {
    match receipt.source_type {
        ReceiptSourceType::Registry => {
            let label = match (receipt.org.as_deref(), receipt.version.as_deref()) {
                (Some(org), Some(ver)) => format!("{org}/{}@{ver}", receipt.skill_name),
                (Some(org), None) => format!("{org}/{}", receipt.skill_name),
                (None, Some(ver)) => format!("{}@{ver}", receipt.skill_name),
                (None, None) => receipt.skill_name.clone(),
            };
            format!("registry skill `{label}`")
        }
        ReceiptSourceType::Local => format!("local skill `{}`", receipt.skill_name),
    }
}

fn describe_incoming(incoming: &IncomingIdentity<'_>) -> String {
    match incoming.receipt {
        Some((ReceiptSourceType::Registry, Some(org))) => {
            format!("registry skill `{org}/{}`", incoming.skill_name)
        }
        Some((ReceiptSourceType::Registry, None)) => {
            format!("registry skill `{}`", incoming.skill_name)
        }
        Some((ReceiptSourceType::Local, _)) | None => {
            format!("local skill `{}`", incoming.skill_name)
        }
    }
}

fn create_staging_dir(dest_root: &Path, installed_as: &str) -> Result<PathBuf> {
    fs::create_dir_all(dest_root)
        .with_context(|| format!("failed to create `{}`", dest_root.display()))?;

    crate::fs_atomic::create_unique_dir(dest_root, ".agentstack-install-", installed_as)
        .with_context(|| {
            format!(
                "failed to create a unique temporary install directory under `{}`",
                dest_root.display()
            )
        })
}

fn commit_staged_install(
    staging: &Path,
    destination: &Path,
    force: bool,
    replace_matching: bool,
    initial_collision: Collision,
    incoming: &IncomingIdentity<'_>,
) -> Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "`{}` is a symlink; refusing to install into it",
                    destination.display()
                );
            }
            if !metadata.is_dir() {
                bail!("`{}` exists and is not a directory", destination.display());
            }
            let may_overwrite = force || initial_collision == Collision::Match;
            if !may_overwrite {
                bail!(
                    "refusing to overwrite `{}` (rerun with --force to replace)",
                    destination.display()
                );
            }
            verify_destination_still_expected(
                destination,
                replace_matching,
                initial_collision,
                incoming,
            )?;
            replace_existing_install(staging, destination)?;
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            fs::rename(staging, destination).with_context(|| {
                format!(
                    "failed to move `{}` -> `{}`",
                    staging.display(),
                    destination.display()
                )
            })?;
            Ok(false)
        }
        Err(err) => Err(err).with_context(|| format!("failed to stat `{}`", destination.display())),
    }
}

fn verify_destination_still_expected(
    destination: &Path,
    replace_matching: bool,
    initial_collision: Collision,
    incoming: &IncomingIdentity<'_>,
) -> Result<()> {
    match initial_collision {
        Collision::Match => {
            let existing = load_existing_receipt(destination)?;
            let Some(receipt) = existing.as_ref() else {
                bail!(
                    "destination `{}` changed during install; refusing to overwrite it. rerun the command after checking the destination.",
                    destination.display()
                );
            };
            if !identity_matches(incoming, receipt) {
                bail!(
                    "destination `{}` changed during install; refusing to overwrite it. rerun the command after checking the destination.",
                    destination.display()
                );
            }
            ensure_direct_update_target(destination, receipt, replace_matching)
        }
        Collision::None => {
            bail!(
                "destination `{}` appeared during install; refusing to overwrite it. rerun the command after checking the destination.",
                destination.display()
            );
        }
        Collision::Foreign => Ok(()),
    }
}

fn replace_existing_install(staging: &Path, destination: &Path) -> Result<()> {
    let backup = unique_backup_path(destination)?;
    fs::rename(destination, &backup).with_context(|| {
        format!(
            "failed to move existing `{}` -> `{}`",
            destination.display(),
            backup.display()
        )
    })?;

    match fs::rename(staging, destination) {
        Ok(()) => {
            // The new content is already in place; failing to delete the old
            // backup must not turn a completed install into an error.
            let _ = fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(install_err) => {
            let restore = fs::rename(&backup, destination);
            if let Err(restore_err) = restore {
                bail!(
                    "failed to move `{}` -> `{}`: {}; also failed to restore `{}` -> `{}`: {}",
                    staging.display(),
                    destination.display(),
                    install_err,
                    backup.display(),
                    destination.display(),
                    restore_err,
                );
            }
            Err(install_err).with_context(|| {
                format!(
                    "failed to move `{}` -> `{}`",
                    staging.display(),
                    destination.display()
                )
            })
        }
    }
}

fn unique_backup_path(destination: &Path) -> Result<PathBuf> {
    crate::fs_atomic::reserve_sibling_path(destination, ".agentstack-install-backup-").with_context(
        || {
            format!(
                "failed to create a unique temporary backup path next to `{}`",
                destination.display()
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(dir: &Path, name: &str, description: &str) {
        fs::create_dir_all(dir).unwrap();
        let body = format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n# Purpose\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
        );
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn unique_dir(prefix: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "agentstack-install-{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn assert_no_target_lock(root: &Path) {
        assert!(
            !root.join(TARGET_LOCK_DIR).exists(),
            "target lock should be cleaned up"
        );
    }

    fn installed_registry_receipt(skill: &str, org: &str, destination: &Path) -> InstallReceipt {
        InstallReceipt::from_request(
            skill.to_string(),
            destination.to_path_buf(),
            InstallReceiptRequest {
                source_type: ReceiptSourceType::Registry,
                source_ref: format!("{org}/{skill}"),
                registry_url: Some("mock://registry".to_string()),
                org: Some(org.to_string()),
                version: Some("1".to_string()),
                hash: None,
                content_hash: None,
                target: "local".to_string(),
                installed_by: Some("tester".to_string()),
                installed_via: None,
                installed_via_stacks: Vec::new(),
            },
        )
        .unwrap()
    }

    #[test]
    fn target_install_lock_writes_metadata() {
        let scratch = unique_dir("lock-metadata");
        let target_root = scratch.join("target");
        let lock =
            TargetInstallLock::acquire_for_target(&target_root, Some("install"), None).unwrap();

        let metadata = read_target_lock_metadata(&target_root).unwrap();
        assert_eq!(metadata.pid, std::process::id());
        assert_eq!(metadata.command_kind.as_deref(), Some("install"));
        assert_eq!(metadata.target_root, target_root.canonicalize().unwrap());
        assert_eq!(metadata.agentstack.version, env!("CARGO_PKG_VERSION"));
        assert!(metadata.created_at_time().is_some());

        drop(lock);
        assert_no_target_lock(&target_root);
    }

    #[test]
    fn target_busy_error_includes_safe_lock_context() {
        let scratch = unique_dir("lock-busy");
        let target_root = scratch.join("target");
        let _lock =
            TargetInstallLock::acquire_for_target(&target_root, Some("update"), None).unwrap();

        let err = TargetInstallLock::acquire_with_timeout(
            &target_root,
            Some("install"),
            Some("codex"),
            Duration::from_millis(1),
        )
        .unwrap_err();
        let busy = err.downcast_ref::<TargetBusyError>().unwrap();
        let canonical = target_root.canonicalize().unwrap();
        assert_eq!(busy.target_root, canonical);
        assert_eq!(busy.lock_path, canonical.join(TARGET_LOCK_DIR));
        assert_eq!(busy.pid, Some(std::process::id()));
        assert!(busy.lock_age.is_some());
        assert_eq!(
            busy.suggested_next_command,
            "agentstack install doctor --target codex"
        );

        let msg = busy.to_string();
        assert!(msg.contains("target_busy"));
        assert!(msg.contains(".agentstack-install.lock"));
        assert!(!msg.to_ascii_lowercase().contains("token"));
    }

    #[test]
    fn commit_rechecks_matching_destination_identity_before_replace() {
        let scratch = unique_dir("commit-recheck-match");
        let staging = scratch.join("staging");
        let destination = scratch.join("alpha");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("SKILL.md"), b"new").unwrap();
        fs::create_dir_all(&destination).unwrap();
        write_receipt_to_dir(
            &destination,
            &installed_registry_receipt("alpha", "evilcorp", &destination),
        )
        .unwrap();

        let incoming_receipt = installed_registry_receipt("alpha", "acme", &destination);
        let incoming = IncomingIdentity::from_receipt("alpha", Some(&incoming_receipt));
        let err = commit_staged_install(
            &staging,
            &destination,
            false,
            false,
            Collision::Match,
            &incoming,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("changed during install"), "got: {msg}");
        assert!(
            msg.contains("rerun the command after checking the destination."),
            "got: {msg}"
        );
        assert!(!msg.contains("Re-run"), "got: {msg}");
        assert!(destination.join(crate::receipt::RECEIPT_FILE).is_file());
        assert!(staging.join("SKILL.md").is_file());
    }

    #[test]
    fn commit_rechecks_matching_destination_refuses_stack_managed_receipt() {
        let scratch = unique_dir("commit-recheck-stack");
        let staging = scratch.join("staging");
        let destination = scratch.join("alpha");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("SKILL.md"), b"new").unwrap();
        fs::create_dir_all(&destination).unwrap();

        let mut existing_receipt = installed_registry_receipt("alpha", "acme", &destination);
        existing_receipt.installed_via_stacks.push(stack_referrer());
        write_receipt_to_dir(&destination, &existing_receipt).unwrap();

        let incoming_receipt = installed_registry_receipt("alpha", "acme", &destination);
        let incoming = IncomingIdentity::from_receipt("alpha", Some(&incoming_receipt));
        let err = commit_staged_install(
            &staging,
            &destination,
            false,
            true,
            Collision::Match,
            &incoming,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("stack-managed install"), "got: {msg}");
        assert!(destination.join(crate::receipt::RECEIPT_FILE).is_file());
        assert!(staging.join("SKILL.md").is_file());
    }

    #[test]
    fn commit_refuses_destination_that_appears_after_empty_precheck() {
        let scratch = unique_dir("commit-recheck-none");
        let staging = scratch.join("staging");
        let destination = scratch.join("alpha");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("SKILL.md"), b"new").unwrap();
        fs::create_dir_all(&destination).unwrap();

        let incoming_receipt = installed_registry_receipt("alpha", "acme", &destination);
        let incoming = IncomingIdentity::from_receipt("alpha", Some(&incoming_receipt));
        let err = commit_staged_install(
            &staging,
            &destination,
            true,
            false,
            Collision::None,
            &incoming,
        )
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(msg.contains("appeared during install"), "got: {msg}");
        assert!(
            msg.contains("rerun the command after checking the destination."),
            "got: {msg}"
        );
        assert!(!msg.contains("Re-run"), "got: {msg}");
        assert!(destination.is_dir());
        assert!(staging.join("SKILL.md").is_file());
    }

    #[test]
    fn concurrent_same_target_skill_installs_serialize_cleanly() {
        let scratch = unique_dir("concurrent-skill");
        let source = scratch.join("src/alpha");
        let dest_root = scratch.join("target");
        make_skill(&source, "alpha", "Use when alpha tasks come up");

        thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..5 {
                let source = &source;
                let dest_root = &dest_root;
                handles.push(scope.spawn(move || {
                    install_skill(InstallOptions {
                        source,
                        dest_root,
                        name_override: None,
                        force: true,
                        replace_matching: false,
                        receipt: None,
                    })
                    .map(|report| report.destination)
                }));
            }
            for handle in handles {
                let destination = handle.join().unwrap().unwrap();
                assert_eq!(destination, dest_root.join("alpha"));
            }
        });

        assert!(dest_root.join("alpha/SKILL.md").is_file());
        assert_no_target_lock(&dest_root);
    }

    #[test]
    fn install_copies_skill_directory() {
        let scratch = unique_dir("copy");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha");

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        assert_eq!(report.installed_as, "alpha");
        assert_eq!(report.destination, dest_root.join("alpha"));
        assert!(report.destination.join("SKILL.md").is_file());
        assert!(!report.overwrote_existing);
        assert!(report.files_copied >= 1);
    }

    #[test]
    fn install_refuses_overwrite_without_force() {
        let scratch = unique_dir("noforce");
        let source = scratch.join("beta");
        let dest_root = scratch.join("targets");
        make_skill(&source, "beta", "Use when beta");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
    }

    #[test]
    fn install_force_overwrites() {
        let scratch = unique_dir("force");
        let source = scratch.join("gamma");
        let dest_root = scratch.join("targets");
        make_skill(&source, "gamma", "Use when gamma");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        // Drop a sentinel file inside the existing install to confirm it goes away.
        let sentinel = dest_root.join("gamma").join("stale.txt");
        fs::write(&sentinel, b"old").unwrap();

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(!sentinel.exists());
    }

    #[test]
    fn install_force_can_replace_from_existing_destination() {
        let scratch = unique_dir("force-self");
        let source = scratch.join("zeta");
        let dest_root = scratch.join("targets");
        make_skill(&source, "zeta", "Use when zeta");

        let first = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        let report = install_skill(InstallOptions {
            source: &first.destination,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: None,
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(report.destination.join("SKILL.md").is_file());
    }

    #[test]
    fn install_invalid_skill_fails_before_writing() {
        let scratch = unique_dir("invalid");
        let source = scratch.join("broken");
        let dest_root = scratch.join("targets");
        fs::create_dir_all(&source).unwrap();
        // No SKILL.md → validation must fail.

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("not a valid skill"));
        assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
    }

    #[test]
    fn install_rejects_name_override() {
        let scratch = unique_dir("alias");
        let source = scratch.join("delta");
        let dest_root = scratch.join("targets");
        make_skill(&source, "delta", "Use when delta");

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: Some("delta-edge"),
            force: false,
            replace_matching: false,
            receipt: None,
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("install aliases are not supported yet")
        );
        assert!(!dest_root.exists() || fs::read_dir(&dest_root).unwrap().next().is_none());
    }

    fn add_overlay(dir: &Path, platform: &str, name: &str, marker: &str) {
        let overlay = dir.join(PLATFORM_DIR).join(platform);
        fs::create_dir_all(overlay.join("references")).unwrap();
        let body = format!(
            "---\nname: {name}\ndescription: Use when {name} is needed\n---\n\n# Purpose\n\n{marker}\n\n# When to Use\n\n# Instructions\n\n# Output\n\n# Boundaries\n"
        );
        fs::write(overlay.join("SKILL.md"), body).unwrap();
        fs::write(overlay.join("references").join("extra.md"), marker).unwrap();
    }

    #[test]
    fn install_applies_platform_overlay_for_matching_target() {
        let scratch = unique_dir("overlay-match");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha is needed");
        add_overlay(&source, "claude-code", "alpha", "claude-code adaptation");

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let overlay = report.overlay.as_ref().expect("overlay should be applied");
        assert_eq!(overlay.platform, "claude-code");
        assert_eq!(overlay.files, 2);

        let installed_manifest = fs::read_to_string(report.destination.join("SKILL.md")).unwrap();
        assert!(
            installed_manifest.contains("claude-code adaptation"),
            "overlay SKILL.md should replace the base file"
        );
        assert!(
            report
                .destination
                .join("references")
                .join("extra.md")
                .is_file()
        );
        // The platform directory itself stays in the installed copy verbatim.
        assert!(
            report
                .destination
                .join("platform/claude-code/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn install_skips_overlay_for_local_target() {
        let scratch = unique_dir("overlay-local");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha is needed");
        add_overlay(&source, "claude-code", "alpha", "claude-code adaptation");

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "local")),
        })
        .unwrap();

        assert!(report.overlay.is_none(), "local target has no platform");
        let installed_manifest = fs::read_to_string(report.destination.join("SKILL.md")).unwrap();
        assert!(
            !installed_manifest.contains("claude-code adaptation"),
            "base SKILL.md should be untouched"
        );
        assert!(
            report
                .destination
                .join("platform/claude-code/SKILL.md")
                .is_file()
        );
    }

    #[test]
    fn install_content_hash_reflects_post_overlay_tree() {
        let scratch = unique_dir("overlay-hash");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha is needed");
        add_overlay(&source, "claude-code", "alpha", "claude-code adaptation");

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();
        assert!(report.overlay.is_some());

        let receipt = read_receipt_from_dir(&report.destination).unwrap();
        let actual = hash_installable_tree_at(&report.destination).unwrap();
        assert_eq!(
            receipt.content_hash.as_deref(),
            Some(crate::receipt::format_hash(&actual).as_str()),
            "content_hash must match the final installed (post-overlay) tree"
        );
    }

    #[test]
    fn local_install_receipt_records_content_hash() {
        let scratch = unique_dir("local-content-hash");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha is needed");

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(local_receipt("local", &source)),
        })
        .unwrap();

        let receipt = read_receipt_from_dir(&report.destination).unwrap();
        let actual = hash_installable_tree_at(&report.destination).unwrap();
        assert_eq!(
            receipt.content_hash.as_deref(),
            Some(crate::receipt::format_hash(&actual).as_str()),
            "local installs must record a content_hash for drift detection"
        );
    }

    #[test]
    fn install_rejects_overlay_that_renames_the_skill() {
        let scratch = unique_dir("overlay-rename");
        let source = scratch.join("alpha");
        let dest_root = scratch.join("targets");
        make_skill(&source, "alpha", "Use when alpha is needed");
        // Overlay manifest claims a different skill name.
        add_overlay(&source, "claude-code", "other", "claude-code adaptation");

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("produced an invalid skill"),
            "got: {err:#}"
        );
        assert!(!dest_root.join("alpha").exists());
    }

    fn registry_receipt(org: &str, target: &str) -> InstallReceiptRequest {
        InstallReceiptRequest {
            source_type: ReceiptSourceType::Registry,
            source_ref: format!("{org}/skill"),
            registry_url: Some("https://example.invalid".into()),
            org: Some(org.into()),
            version: Some("1.0.0".into()),
            hash: None,
            content_hash: None,
            target: target.into(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }
    }

    fn stack_referrer() -> crate::receipt::InstallVia {
        crate::receipt::InstallVia {
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "platform".to_string(),
            manifest_hash: "sha256:abc123".to_string(),
        }
    }

    fn stack_managed_registry_receipt(org: &str, target: &str) -> InstallReceiptRequest {
        let mut receipt = registry_receipt(org, target);
        receipt.installed_via_stacks.push(stack_referrer());
        receipt
    }

    fn local_receipt(target: &str, source: &Path) -> InstallReceiptRequest {
        InstallReceiptRequest {
            source_type: ReceiptSourceType::Local,
            source_ref: source.display().to_string(),
            registry_url: None,
            org: None,
            version: None,
            hash: None,
            content_hash: None,
            target: target.into(),
            installed_by: None,
            installed_via: None,
            installed_via_stacks: Vec::new(),
        }
    }

    #[test]
    fn reinstall_same_registry_skill_requires_force() {
        let scratch = unique_dir("collide-registry-match");
        let source = scratch.join("kappa");
        let dest_root = scratch.join("targets");
        make_skill(&source, "kappa", "Use when kappa");

        let first = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();
        assert!(first.receipt.is_some());

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("refusing to replace existing install"));
        assert!(msg.contains("--force"));
        assert!(first.destination.join("SKILL.md").is_file());
    }

    #[test]
    fn replace_matching_registry_skill_succeeds_without_force_for_update() {
        let scratch = unique_dir("collide-registry-update-match");
        let source = scratch.join("kappa");
        let dest_root = scratch.join("targets");
        make_skill(&source, "kappa", "Use when kappa");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: true,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(!report.replaced_foreign);
    }

    #[test]
    fn replace_matching_registry_skill_refuses_stack_managed_existing_receipt() {
        let scratch = unique_dir("collide-registry-update-stack");
        let source = scratch.join("kappa");
        let dest_root = scratch.join("targets");
        make_skill(&source, "kappa", "Use when kappa");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(stack_managed_registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: true,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("stack-managed install"), "msg = {msg}");
        assert!(msg.contains("stack update"), "msg = {msg}");
        assert!(dest_root.join("kappa/SKILL.md").is_file());
    }

    #[test]
    fn install_force_replaces_stack_managed_receipt_when_not_replace_matching() {
        let scratch = unique_dir("collide-registry-force-stack");
        let source = scratch.join("kappa");
        let dest_root = scratch.join("targets");
        make_skill(&source, "kappa", "Use when kappa");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(stack_managed_registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let sentinel = dest_root.join("kappa/stale.txt");
        fs::write(&sentinel, b"old").unwrap();

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(!report.replaced_foreign);
        assert!(!sentinel.exists());
        assert!(report.destination.join("SKILL.md").is_file());
    }

    #[test]
    fn replace_matching_registry_skill_refuses_stack_managed_existing_receipt_with_force() {
        let scratch = unique_dir("collide-registry-update-stack-force");
        let source = scratch.join("kappa");
        let dest_root = scratch.join("targets");
        make_skill(&source, "kappa", "Use when kappa");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(stack_managed_registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: true,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("stack-managed install"), "msg = {msg}");
        assert!(dest_root.join("kappa/SKILL.md").is_file());
    }

    #[test]
    fn install_different_org_into_same_dir_fails_without_force() {
        let scratch = unique_dir("collide-foreign-org");
        let source = scratch.join("lambda");
        let dest_root = scratch.join("targets");
        make_skill(&source, "lambda", "Use when lambda");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("evilcorp", "claude-code")),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refusing to overwrite"), "msg = {msg}");
        assert!(msg.contains("acme/lambda"), "msg = {msg}");
        assert!(msg.contains("evilcorp/lambda"), "msg = {msg}");
        assert!(msg.contains("--force"), "msg = {msg}");
    }

    #[test]
    fn install_into_dir_without_receipt_fails_without_force() {
        let scratch = unique_dir("collide-no-receipt");
        let source = scratch.join("mu");
        let dest_root = scratch.join("targets");
        make_skill(&source, "mu", "Use when mu");

        // Pre-create the destination as a foreign / user-authored directory:
        // a real dir but no AgentStack install receipt.
        let dest = dest_root.join("mu");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("hello.txt"), b"foreign").unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refusing to overwrite"), "msg = {msg}");
        assert!(msg.contains("unmanaged"), "msg = {msg}");
        assert!(msg.contains("--force"), "msg = {msg}");
    }

    #[test]
    fn force_replaces_foreign_install_and_flags_replaced_foreign() {
        let scratch = unique_dir("collide-foreign-force");
        let source = scratch.join("nu");
        let dest_root = scratch.join("targets");
        make_skill(&source, "nu", "Use when nu");

        let dest = dest_root.join("nu");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("legacy.txt"), b"foreign").unwrap();

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(report.replaced_foreign);
        assert!(!report.destination.join("legacy.txt").exists());
    }

    #[test]
    fn local_to_local_same_name_requires_force() {
        let scratch = unique_dir("collide-local-match");
        let source = scratch.join("xi");
        let dest_root = scratch.join("targets");
        make_skill(&source, "xi", "Use when xi");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(local_receipt("claude-code", &source)),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(local_receipt("claude-code", &source)),
        })
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("refusing to replace existing install"));
        assert!(msg.contains("--force"));
    }

    #[test]
    fn local_after_registry_fails_without_force() {
        let scratch = unique_dir("collide-local-vs-registry");
        let source = scratch.join("omicron");
        let dest_root = scratch.join("targets");
        make_skill(&source, "omicron", "Use when omicron");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(local_receipt("claude-code", &source)),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refusing to overwrite"), "msg = {msg}");
        assert!(msg.contains("registry skill"), "msg = {msg}");
        assert!(msg.contains("local skill"), "msg = {msg}");
    }

    #[test]
    fn registry_after_local_fails_without_force() {
        let scratch = unique_dir("collide-registry-vs-local");
        let source = scratch.join("pi");
        let dest_root = scratch.join("targets");
        make_skill(&source, "pi", "Use when pi");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(local_receipt("claude-code", &source)),
        })
        .unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refusing to overwrite"), "msg = {msg}");
        assert!(msg.contains("local skill"), "msg = {msg}");
        assert!(msg.contains("registry skill"), "msg = {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_destination_that_is_a_symlink() {
        let scratch = unique_dir("symlink-dest");
        let source = scratch.join("sigma");
        let dest_root = scratch.join("targets");
        make_skill(&source, "sigma", "Use when sigma");

        // Pre-create a symlinked destination pointing at a real directory the
        // attacker controls outside the install root.
        let outside = scratch.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&dest_root).unwrap();
        let link = dest_root.join("sigma");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: None,
        })
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("symlink"), "msg = {msg}");
        // The symlink and its target must be untouched.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(outside.is_dir());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn force_replaces_foreign_org_and_flags_replaced_foreign() {
        let scratch = unique_dir("collide-foreign-org-force");
        let source = scratch.join("rho");
        let dest_root = scratch.join("targets");
        make_skill(&source, "rho", "Use when rho");

        install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: false,
            replace_matching: false,
            receipt: Some(registry_receipt("acme", "claude-code")),
        })
        .unwrap();

        let report = install_skill(InstallOptions {
            source: &source,
            dest_root: &dest_root,
            name_override: None,
            force: true,
            replace_matching: false,
            receipt: Some(registry_receipt("evilcorp", "claude-code")),
        })
        .unwrap();

        assert!(report.overwrote_existing);
        assert!(report.replaced_foreign);
    }
}
