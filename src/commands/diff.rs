//! `agentstack skill diff` — compare package contents for local, registry, or
//! installed skills.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::refs::SkillRefInput;
use super::{
    client::{configured_client, registry_context},
    refs,
};
use crate::config::ConfigStore;
use crate::error::CliError;
use crate::output::Ctx;
use crate::package::{PackageHash, PackageManifest, build_skill_package, unpack_verified_bytes};
use crate::receipt::{InstallReceipt, read_receipt_from_dir};
use crate::registry::{PullClientOptions, RegistryClient};
use crate::skill_ref::SkillRef;
use crate::targets::{InstallTarget, TargetResolver};

pub struct Args {
    pub left: String,
    pub right: Option<String>,
    pub target: Option<String>,
    pub allow_yanked: bool,
}

pub struct DiffOptions<'a> {
    pub left: &'a str,
    pub right: &'a str,
    pub json: bool,
    pub quiet: bool,
    pub allow_yanked: bool,
}

pub struct InstalledDiffOptions<'a> {
    pub skill_ref: &'a str,
    pub target: InstallTarget,
    pub target_root: &'a Path,
    pub json: bool,
    pub quiet: bool,
    pub allow_yanked: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffOutcome {
    pub left: DiffSourceSummary,
    pub right: DiffSourceSummary,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<ChangedFile>,
    pub unchanged_count: usize,
    pub changed_count: usize,
    pub is_empty: bool,
}

#[derive(Debug, Serialize)]
pub struct DiffSourceSummary {
    pub source_type: &'static str,
    pub source: String,
    pub name: String,
    pub version: Option<String>,
    pub hash: PackageHash,
    pub file_count: usize,
}

#[derive(Debug, Serialize)]
pub struct ChangedFile {
    pub path: String,
    pub left_sha256: String,
    pub right_sha256: String,
}

struct DiffSource {
    summary: DiffSourceSummary,
    /// Package-relative path -> sha256 hex digest.
    files: BTreeMap<String, String>,
    _temp_dir: Option<TempDir>,
}

struct TempDir {
    path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    if let Some(target) = args.target.as_deref() {
        return run_installed(ctx, &args.left, target, args.allow_yanked);
    }
    let right = args
        .right
        .as_deref()
        .context("missing right side; pass two refs, or use `--target <TARGET>` to compare an installed copy")?;

    if looks_like_local_path(&args.left) && looks_like_local_path(right) {
        let left = load_local(&args.left)?;
        let right = load_local(right)?;
        let outcome = compare_sources(left, right);
        render_outcome(&outcome, ctx.json, ctx.quiet)?;
        return Ok(());
    }

    preflight_remote_source(&args.left, args.allow_yanked)?;
    preflight_remote_source(right, args.allow_yanked)?;
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    let left = load_source(
        &configured.client,
        Some(ctx),
        Some(&configured.url),
        &args.left,
        args.allow_yanked,
    )?;
    let right = load_source(
        &configured.client,
        Some(ctx),
        Some(&configured.url),
        right,
        args.allow_yanked,
    )?;
    let outcome = compare_sources(left, right);
    render_outcome(&outcome, ctx.json, ctx.quiet)?;
    Ok(())
}

fn run_installed(ctx: &Ctx, raw: &str, target: &str, allow_yanked: bool) -> Result<()> {
    let target = InstallTarget::parse(target)?;
    let store = ConfigStore::load().context("failed to load config")?;
    let resolver = TargetResolver::new(&store);
    let resolved = resolver.resolve(target)?;
    // Validate the ref shape and find the install receipt before any
    // registry or auth lookup so missing installs fail fast and offline.
    resolve_installed_sides(raw, target, &resolved.path, allow_yanked)?;

    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));
    run_installed_with_client(
        &configured.client,
        Some(&configured.url),
        InstalledDiffOptions {
            skill_ref: raw,
            target,
            target_root: &resolved.path,
            json: ctx.json,
            quiet: ctx.quiet,
            allow_yanked,
        },
    )?;
    Ok(())
}

pub fn run_installed_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    opts: InstalledDiffOptions<'_>,
) -> Result<DiffOutcome> {
    let (installed_path, receipt, remote_ref) = resolve_installed_sides(
        opts.skill_ref,
        opts.target,
        opts.target_root,
        opts.allow_yanked,
    )?;
    let left = load_installed(&installed_path, &receipt)?;
    let mut right = load_remote(
        client,
        registry_url,
        &remote_ref.to_string(),
        &remote_ref,
        opts.allow_yanked,
    )?;
    // The installed side carries any applied platform overlay; project the
    // same overlay onto the registry side so the diff reflects what install
    // would actually write into this target.
    project_platform_overlay(&mut right.files, opts.target.platform());
    let outcome = compare_sources(left, right);
    render_outcome(&outcome, opts.json, opts.quiet)?;
    Ok(outcome)
}

fn resolve_installed_sides(
    raw: &str,
    target: InstallTarget,
    target_root: &Path,
    allow_yanked: bool,
) -> Result<(PathBuf, InstallReceipt, SkillRef)> {
    let input = refs::validate_skill_ref_input(raw)?;
    if allow_yanked && input.version().is_none() {
        bail!(
            "--allow-yanked requires explicit pinned refs such as `skill@version` or `org/skill@version`"
        );
    }
    let (ref_org, name, version) = match input {
        SkillRefInput::Qualified(skill_ref) => (
            Some(skill_ref.org.clone()),
            skill_ref.name.clone(),
            skill_ref.version.clone(),
        ),
        SkillRefInput::Relative { name, version } => (None, name, version),
    };

    let installed_path = target_root.join(&name);
    let receipt = read_receipt_from_dir(&installed_path).map_err(|_| {
        CliError::new(
            "install_receipt_missing",
            format!("no install receipt for `{}`", installed_path.display()),
        )
        .resource(installed_path.display().to_string())
        .action("diff")
        .next_command(format!(
            "agentstack install list --target {}",
            target.as_str()
        ))
    })?;

    let receipt_org = receipt
        .org
        .clone()
        .or_else(|| SkillRef::parse(&receipt.source_ref).ok().map(|r| r.org));
    let org = match ref_org {
        Some(org) => {
            if let Some(receipt_org) = &receipt_org
                && receipt_org != &org
            {
                bail!(
                    "installed `{name}` in target `{}` came from org `{receipt_org}`, not `{org}`",
                    target.as_str()
                );
            }
            org
        }
        None => receipt_org.with_context(|| {
            format!(
                "install receipt for `{name}` does not record a registry org; use `org/{name}` to pick the registry side"
            )
        })?,
    };

    let mut remote_ref = SkillRef::new(org, name)?;
    if let Some(version) = version {
        remote_ref = remote_ref.with_version(version)?;
    }
    Ok((installed_path, receipt, remote_ref))
}

fn preflight_remote_source(raw: &str, allow_yanked: bool) -> Result<()> {
    if looks_like_local_path(raw) {
        return Ok(());
    }
    let input = refs::validate_skill_ref_input(raw)?;
    check_allow_yanked_pinned(allow_yanked, input.version().is_some())
}

fn check_allow_yanked_pinned(allow_yanked: bool, has_pinned_version: bool) -> Result<()> {
    if allow_yanked && !has_pinned_version {
        bail!(
            "--allow-yanked requires explicit pinned refs such as `skill@version` or `org/skill@version`"
        );
    }
    Ok(())
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    opts: DiffOptions<'_>,
) -> Result<DiffOutcome> {
    let left = load_source(client, None, registry_url, opts.left, opts.allow_yanked)?;
    let right = load_source(client, None, registry_url, opts.right, opts.allow_yanked)?;
    let outcome = compare_sources(left, right);
    render_outcome(&outcome, opts.json, opts.quiet)?;

    Ok(outcome)
}

fn render_outcome(outcome: &DiffOutcome, json: bool, quiet: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
    } else if !quiet {
        render_human(outcome);
    }
    Ok(())
}

fn load_source(
    client: &dyn RegistryClient,
    ctx: Option<&Ctx>,
    registry_url: Option<&str>,
    raw: &str,
    allow_yanked: bool,
) -> Result<DiffSource> {
    if looks_like_local_path(raw) {
        return load_local(raw);
    }

    let skill_ref: SkillRef = match raw.parse() {
        Ok(skill_ref) => skill_ref,
        Err(err) => match ctx {
            Some(ctx) => refs::resolve_skill_ref(ctx, client, raw)?,
            None => return Err(err.into()),
        },
    };
    check_allow_yanked_pinned(allow_yanked, skill_ref.version.is_some())?;
    load_remote(client, registry_url, raw, &skill_ref, allow_yanked)
}

fn looks_like_local_path(raw: &str) -> bool {
    let path = Path::new(raw);
    path.exists() || raw.starts_with('.') || raw.starts_with('/') || raw.starts_with('~')
}

fn load_local(raw: &str) -> Result<DiffSource> {
    let source = Path::new(raw);
    let built = build_skill_package(source)?;
    let files = digest_manifest_files(source, &built.manifest)?;
    Ok(DiffSource {
        summary: DiffSourceSummary {
            source_type: "path",
            source: raw.to_string(),
            name: built.manifest.name,
            version: None,
            hash: built.hash,
            file_count: files.len(),
        },
        files,
        _temp_dir: None,
    })
}

fn load_installed(path: &Path, receipt: &InstallReceipt) -> Result<DiffSource> {
    let built = build_skill_package(path)
        .with_context(|| format!("failed to read installed skill at `{}`", path.display()))?;
    let files = digest_manifest_files(path, &built.manifest)?;
    Ok(DiffSource {
        summary: DiffSourceSummary {
            source_type: "installed",
            source: path.display().to_string(),
            name: built.manifest.name,
            version: receipt.version.clone(),
            hash: built.hash,
            file_count: files.len(),
        },
        files,
        _temp_dir: None,
    })
}

fn load_remote(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    raw: &str,
    skill_ref: &SkillRef,
    allow_yanked: bool,
) -> Result<DiffSource> {
    let response = client
        .pull_with_options(skill_ref, PullClientOptions { allow_yanked })
        .with_context(|| registry_context(registry_url, "download from", "registry download"))?;
    let actual = PackageHash::sha256_of(&response.archive);
    if actual != response.metadata.hash {
        bail!(
            "hash mismatch for {}: expected {} but archive bytes hash to {}",
            response.metadata.skill_ref(),
            response.metadata.hash.hex,
            actual.hex,
        );
    }

    let temp_dir = TempDir {
        path: unique_temp_dir("agentstack-diff")?,
    };
    let unpacked = unpack_verified_bytes(&response.archive, &temp_dir.path, false, actual)
        .with_context(|| "failed to unpack registry archive for diff")?;
    let files = digest_manifest_files(&unpacked.out_path, &unpacked.manifest)?;

    Ok(DiffSource {
        summary: DiffSourceSummary {
            source_type: "registry",
            source: raw.to_string(),
            name: response.metadata.name,
            version: Some(response.metadata.version),
            hash: response.metadata.hash,
            file_count: files.len(),
        },
        files,
        _temp_dir: Some(temp_dir),
    })
}

fn digest_manifest_files(
    source: &Path,
    manifest: &PackageManifest,
) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for rel in &manifest.files {
        let path = source.join(rel);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read `{}`", path.display()))?;
        files.insert(rel.clone(), PackageHash::sha256_of(&bytes).hex);
    }
    Ok(files)
}

fn compare_sources(left: DiffSource, right: DiffSource) -> DiffOutcome {
    let (added, removed, changed, unchanged_count) = compare_file_maps(&left.files, &right.files);
    let changed_count = added.len() + removed.len() + changed.len();
    DiffOutcome {
        left: left.summary,
        right: right.summary,
        added,
        removed,
        changed,
        unchanged_count,
        changed_count,
        is_empty: changed_count == 0,
    }
}

#[allow(clippy::type_complexity)]
/// Mirror install-time platform overlay semantics on a registry-side file
/// map: each `platform/<platform>/rest` entry overrides `rest` at the skill
/// root, and the `platform/` entries themselves stay in place. Keeps previews
/// and installed diffs consistent with what install would actually write.
fn project_platform_overlay(files: &mut BTreeMap<String, String>, platform: Option<&str>) {
    let Some(platform) = platform else {
        return;
    };
    let prefix = format!("platform/{platform}/");
    let overlays: Vec<(String, String)> = files
        .iter()
        .filter_map(|(path, digest)| {
            path.strip_prefix(&prefix)
                .filter(|rest| !rest.is_empty())
                .map(|rest| (rest.to_string(), digest.clone()))
        })
        .collect();
    for (path, sha256) in overlays {
        files.insert(path, sha256);
    }
}

fn compare_file_maps(
    left: &BTreeMap<String, String>,
    right: &BTreeMap<String, String>,
) -> (Vec<String>, Vec<String>, Vec<ChangedFile>, usize) {
    let mut paths = BTreeSet::new();
    paths.extend(left.keys().cloned());
    paths.extend(right.keys().cloned());

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged_count = 0usize;

    for path in paths {
        match (left.get(&path), right.get(&path)) {
            (None, Some(_)) => added.push(path),
            (Some(_), None) => removed.push(path),
            (Some(l), Some(r)) if l != r => changed.push(ChangedFile {
                path,
                left_sha256: l.clone(),
                right_sha256: r.clone(),
            }),
            (Some(_), Some(_)) => unchanged_count += 1,
            (None, None) => {}
        }
    }

    (added, removed, changed, unchanged_count)
}

/// File-level change lists between an installed skill directory and an
/// already-unpacked registry archive. Reused by `skill update --check` to
/// preview what an update would touch.
#[derive(Debug, Clone, Serialize)]
pub struct FileChangeSummary {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub unchanged: usize,
}

pub(crate) fn file_changes_between_dirs(
    installed_dir: &Path,
    new_dir: &Path,
    new_manifest: &PackageManifest,
    platform: Option<&str>,
) -> Result<FileChangeSummary> {
    let installed = build_skill_package(installed_dir).with_context(|| {
        format!(
            "failed to read installed skill at `{}`",
            installed_dir.display()
        )
    })?;
    let left = digest_manifest_files(installed_dir, &installed.manifest)?;
    let mut right = digest_manifest_files(new_dir, new_manifest)?;
    project_platform_overlay(&mut right, platform);
    let (added, removed, changed, unchanged) = compare_file_maps(&left, &right);
    Ok(FileChangeSummary {
        added,
        removed,
        changed: changed.into_iter().map(|file| file.path).collect(),
        unchanged,
    })
}

fn render_human(outcome: &DiffOutcome) {
    println!("skill diff");
    println!(
        "  left:  {} ({}, {})",
        outcome.left.source,
        outcome.left.source_type,
        outcome.left.hash.short()
    );
    println!(
        "  right: {} ({}, {})",
        outcome.right.source,
        outcome.right.source_type,
        outcome.right.hash.short()
    );
    println!(
        "  added: {}  removed: {}  changed: {}  unchanged: {}",
        outcome.added.len(),
        outcome.removed.len(),
        outcome.changed.len(),
        outcome.unchanged_count
    );

    print_paths("added", &outcome.added);
    print_paths("removed", &outcome.removed);
    if !outcome.changed.is_empty() {
        println!("changed:");
        for file in &outcome.changed {
            println!(
                "  {}  {} -> {}",
                file.path,
                short_hash(&file.left_sha256),
                short_hash(&file.right_sha256)
            );
        }
    }
}

fn print_paths(label: &str, paths: &[String]) {
    if paths.is_empty() {
        return;
    }
    println!("{label}:");
    for path in paths {
        println!("  {path}");
    }
}

fn short_hash(hex: &str) -> &str {
    let end = hex.len().min(crate::package::SHORT_HASH_LEN);
    &hex[..end]
}

fn unique_temp_dir(prefix: &str) -> Result<PathBuf> {
    crate::fs_atomic::create_unique_dir(&std::env::temp_dir(), "", prefix)
        .context("failed to create a unique temporary diff directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(tag: &str) -> String {
        tag.to_string()
    }

    #[test]
    fn overlay_projection_overrides_and_adds_root_paths() {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_string(), digest("base"));
        files.insert(
            "platform/claude-code/SKILL.md".to_string(),
            digest("overlay"),
        );
        files.insert("platform/claude-code/EXTRA.md".to_string(), digest("extra"));
        files.insert("platform/codex/SKILL.md".to_string(), digest("codex"));

        project_platform_overlay(&mut files, Some("claude-code"));

        assert_eq!(files["SKILL.md"], "overlay");
        assert_eq!(files["EXTRA.md"], "extra");
        // Non-matching platform dirs and the platform/ entries stay verbatim.
        assert_eq!(files["platform/codex/SKILL.md"], "codex");
        assert_eq!(files["platform/claude-code/SKILL.md"], "overlay");
        assert!(!files.contains_key("platform/"));
    }

    #[test]
    fn overlay_projection_is_a_no_op_without_a_platform() {
        let mut files = BTreeMap::new();
        files.insert("SKILL.md".to_string(), digest("base"));
        files.insert(
            "platform/claude-code/SKILL.md".to_string(),
            digest("overlay"),
        );
        let before: Vec<_> = files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        project_platform_overlay(&mut files, None);

        let after: Vec<_> = files.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(before, after);
    }
}
