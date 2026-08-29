use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::configured_client;
use super::refs;
use crate::cache::{Cache, CacheEntry};
use crate::config::ConfigStore;
use crate::error::CliError;
use crate::install::{
    AppliedOverlay, InstallOptions, InstallReport, TargetInstallLock, describe_existing,
    install_skill, load_existing_receipt,
};
use crate::output::Ctx;
use crate::package::{PackageHash, PackageManifest, unpack_verified_bytes};
use crate::receipt::{
    InstallReceipt, InstallReceiptRequest, InstallVia, RECEIPT_SCHEMA_VERSION, ReceiptSourceType,
    StackInstallReceipt, StackInstallReceiptItem, format_hash, installed_timestamp,
    local_installed_by, push_unique_stack_ref, receipt_path, stack_referrers, write_receipt_to_dir,
    write_stack_receipt,
};
use crate::registry::{
    PullClientOptions, RegistryClient, SkillMetadata, StackResolve, StackResolvedItem,
};
use crate::skill::check_slug;
use crate::skill_ref::{SkillRef, check_version};
use crate::targets::{InstallTarget, TargetResolver, TargetSource, default_target_path};

use super::setup;

pub struct Args {
    pub source: Option<String>,
    pub source_name: Option<String>,
    pub org: Option<String>,
    pub team: Option<String>,
    pub target: Option<String>,
    pub force: bool,
    pub allow_yanked: bool,
}

pub struct RemoteInstallOptions<'a> {
    pub skill_ref: &'a SkillRef,
    pub dest_root: &'a Path,
    pub target: &'a str,
    pub force: bool,
    pub registry_url: Option<&'a str>,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
    pub allow_yanked: bool,
}

pub struct StackInstallOptions<'a> {
    pub org: &'a str,
    pub stack: &'a str,
    pub dest_root: &'a Path,
    pub target: &'a str,
    pub force: bool,
    pub registry_url: Option<&'a str>,
    pub installed_by: Option<String>,
    pub cache_root: Option<&'a Path>,
}

#[derive(Debug)]
pub struct RemoteInstallReport {
    pub install: InstallReport,
    pub metadata: SkillMetadata,
    pub cache_entry: CacheEntry,
}

#[derive(Debug)]
pub struct StackInstallReport {
    pub stack: StackResolve,
    pub installed: Vec<StackInstalledItem>,
    pub stack_receipt_path: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct StackInstalledItem {
    pub skill: String,
    pub version: String,
    pub destination: PathBuf,
    pub receipt_path: PathBuf,
    pub overwrote_existing: bool,
    pub overlay: Option<AppliedOverlay>,
}

enum InstallSource {
    Local(PathBuf),
    Remote { raw: String },
    Stack { raw: String },
}

fn preflight_registry_source(
    source: &InstallSource,
    team: Option<&str>,
    allow_yanked: bool,
) -> Result<()> {
    match source {
        InstallSource::Remote { raw } => {
            let input = refs::validate_skill_ref_input_with_team(raw, team)?;
            if allow_yanked && input.version().is_none() {
                bail!(
                    "--allow-yanked requires an explicit pinned ref such as `skill@version` or `org/skill@version`"
                );
            }
        }
        InstallSource::Stack { .. } => {
            if let Some(team) = team {
                check_slug(team)
                    .map_err(|reason| anyhow::anyhow!("invalid --team `{team}`: {reason}"))?;
            }
            if allow_yanked {
                bail!("--allow-yanked is only valid for skill installs");
            }
        }
        InstallSource::Local(_) => {
            if team.is_some() {
                bail!("--team only applies to registry refs");
            }
            if allow_yanked {
                bail!("--allow-yanked is only valid for registry skill installs");
            }
        }
    }
    Ok(())
}

struct RenderInstall<'a> {
    report: &'a InstallReport,
    target: &'a str,
    target_source: TargetSource,
    source_type: ReceiptSourceType,
    source_ref: String,
    registry_url: Option<String>,
    org: Option<String>,
    version: Option<String>,
    hash: Option<String>,
    cache_package_path: Option<PathBuf>,
    installed_label: String,
    platform_warning: Option<String>,
}

struct ResolvedInstallTarget {
    target: InstallTarget,
    path: PathBuf,
    source: TargetSource,
    pending_registration: bool,
}

#[derive(Serialize)]
struct InstallJson<'a> {
    kind: &'static str,
    operation: &'static str,
    resource: &'a str,
    name: &'a str,
    installed_as: &'a str,
    target: &'a str,
    target_source: &'static str,
    destination: &'a Path,
    source_type: &'static str,
    source_ref: &'a str,
    registry_url: Option<&'a str>,
    org: Option<&'a str>,
    version: Option<&'a str>,
    hash: Option<&'a str>,
    hash_kind: Option<&'static str>,
    receipt: Option<&'a PathBuf>,
    cache_package: Option<&'a PathBuf>,
    files: usize,
    overwrote: bool,
    overlay: Option<&'a AppliedOverlay>,
    platform_warning: Option<&'a str>,
    warnings: &'a [String],
    next_commands: Vec<String>,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let source_arg = match args.source {
        Some(path) => path,
        None => bail!(
            "tell agentstack what to install: a local skill directory, `skill[@version]`, or `org/skill[@version]`. \
             Try `agentstack skill scan`, `agentstack skill search <query>`, or \
             `agentstack skill list` first."
        ),
    };

    let source = parse_source(
        &source_arg,
        args.source_name.as_deref(),
        args.org.as_deref(),
    )?;
    preflight_registry_source(&source, args.team.as_deref(), args.allow_yanked)?;
    let resolved = resolve_install_target(ctx, args.target.as_deref())?;
    let target = resolved.target;

    match source {
        InstallSource::Remote { raw } => {
            let configured = configured_client()?;
            ctx.verbose(format!("registry: {}", configured.url));
            let skill_ref = refs::resolve_skill_ref_with_team(
                ctx,
                &configured.client,
                &raw,
                args.team.as_deref(),
            )?;
            let installed_by = configured.client.whoami().ok().map(|reply| reply.user);

            let remote_report = run_remote_with_client(
                &configured.client,
                RemoteInstallOptions {
                    skill_ref: &skill_ref,
                    dest_root: &resolved.path,
                    target: target.as_str(),
                    force: args.force,
                    registry_url: Some(&configured.url),
                    installed_by,
                    cache_root: None,
                    allow_yanked: args.allow_yanked,
                },
            )
            .with_context(|| format!("failed to install `{skill_ref}`"))?;

            commit_pending_target_registration(ctx, &resolved)?;
            warn_if_foreign_overwrite(ctx, &remote_report.install);
            let platform_warning =
                platform_mismatch_warning(&remote_report.metadata.platform_tags, target);
            warn_platform_mismatch(ctx, platform_warning.as_deref());

            let render = RenderInstall {
                report: &remote_report.install,
                target: target.as_str(),
                target_source: resolved.source,
                source_type: ReceiptSourceType::Registry,
                source_ref: unversioned_metadata_ref(&remote_report.metadata),
                registry_url: Some(configured.url),
                org: Some(remote_report.metadata.org.clone()),
                version: Some(remote_report.metadata.version.clone()),
                hash: Some(format_hash(&remote_report.metadata.hash)),
                cache_package_path: Some(remote_report.cache_entry.package_path),
                installed_label: remote_report.metadata.skill_ref(),
                platform_warning,
            };
            render_success(ctx, &render)
        }
        InstallSource::Stack { raw } => {
            let configured = configured_client()?;
            ctx.verbose(format!("registry: {}", configured.url));
            let (org, stack) = refs::resolve_stack_ref_with_team(
                ctx,
                &configured.client,
                &raw,
                None,
                args.team.as_deref(),
            )?;
            let installed_by = configured.client.whoami().ok().map(|reply| reply.user);

            let report = run_stack_with_client(
                &configured.client,
                StackInstallOptions {
                    org: &org,
                    stack: &stack,
                    dest_root: &resolved.path,
                    target: target.as_str(),
                    force: args.force,
                    registry_url: Some(&configured.url),
                    installed_by,
                    cache_root: None,
                },
            )
            .with_context(|| format!("failed to install stack `{org}/{stack}`"))?;

            commit_pending_target_registration(ctx, &resolved)?;
            render_stack_success(ctx, &report, target.as_str(), resolved.source)
        }
        InstallSource::Local(path) => {
            if args.team.is_some() {
                bail!("--team only applies to registry refs");
            }
            commit_pending_target_registration(ctx, &resolved)?;
            let source_ref = local_source_ref(&path);
            let receipt = InstallReceiptRequest {
                source_type: ReceiptSourceType::Local,
                source_ref: source_ref.clone(),
                registry_url: None,
                org: None,
                version: None,
                hash: None,
                content_hash: None,
                target: target.as_str().to_string(),
                installed_by: local_installed_by(),
                installed_via: None,
                installed_via_stacks: Vec::new(),
            };
            let report = install_skill(InstallOptions {
                source: &path,
                dest_root: &resolved.path,
                name_override: None,
                force: args.force,
                replace_matching: false,
                receipt: Some(receipt),
            })
            .with_context(|| format!("failed to install `{}`", path.display()))?;

            warn_if_foreign_overwrite(ctx, &report);

            let render = RenderInstall {
                report: &report,
                target: target.as_str(),
                target_source: resolved.source,
                source_type: ReceiptSourceType::Local,
                source_ref,
                registry_url: None,
                org: None,
                version: None,
                hash: report.receipt.as_ref().and_then(|r| r.hash.clone()),
                cache_package_path: None,
                installed_label: report.manifest_name.clone(),
                platform_warning: None,
            };
            render_success(ctx, &render)
        }
    }
}

pub fn run_remote_with_client(
    client: &dyn RegistryClient,
    opts: RemoteInstallOptions<'_>,
) -> Result<RemoteInstallReport> {
    run_remote_with_replace(client, opts, false)
}

pub(crate) fn run_remote_update_with_client(
    client: &dyn RegistryClient,
    opts: RemoteInstallOptions<'_>,
) -> Result<RemoteInstallReport> {
    run_remote_with_replace(client, opts, true)
}

fn run_remote_with_replace(
    client: &dyn RegistryClient,
    opts: RemoteInstallOptions<'_>,
    replace_matching: bool,
) -> Result<RemoteInstallReport> {
    if opts.allow_yanked && opts.skill_ref.version.is_none() {
        bail!("--allow-yanked requires an explicit pinned ref such as `org/skill@version`");
    }
    let response = client
        .pull_with_options(
            opts.skill_ref,
            PullClientOptions {
                allow_yanked: opts.allow_yanked,
            },
        )
        .with_context(|| match opts.registry_url {
            Some(url) => format!("download from {url} failed"),
            None => "registry download failed".to_string(),
        })?;
    validate_remote_metadata(&response.metadata, opts.skill_ref)?;

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
    let result = install_remote_from_response(
        &cache,
        &stage_parent,
        response,
        actual,
        opts,
        replace_matching,
    );
    let _ = fs::remove_dir_all(&stage_parent);
    result
}

pub fn run_stack_with_client(
    client: &dyn RegistryClient,
    opts: StackInstallOptions<'_>,
) -> Result<StackInstallReport> {
    let resolved = resolve_validated_stack(client, &opts)?;

    let _lock =
        TargetInstallLock::acquire_for_target(opts.dest_root, Some("install"), Some(opts.target))?;
    run_stack_with_resolved(client, opts, &resolved)
}

pub(crate) fn run_stack_with_client_unlocked(
    client: &dyn RegistryClient,
    opts: StackInstallOptions<'_>,
) -> Result<StackInstallReport> {
    let resolved = resolve_validated_stack(client, &opts)?;
    run_stack_with_resolved(client, opts, &resolved)
}

fn resolve_validated_stack(
    client: &dyn RegistryClient,
    opts: &StackInstallOptions<'_>,
) -> Result<StackResolve> {
    let resolved = client
        .resolve_stack(opts.org, opts.stack)
        .with_context(|| format!("resolve stack {}/{} failed", opts.org, opts.stack))?;
    validate_stack_resolution(
        &resolved,
        opts.org,
        opts.stack,
        &format!("`{}/{}`", opts.org, opts.stack),
    )?;
    Ok(resolved)
}

fn run_stack_with_resolved(
    client: &dyn RegistryClient,
    opts: StackInstallOptions<'_>,
    resolved: &StackResolve,
) -> Result<StackInstallReport> {
    let cache = match opts.cache_root {
        Some(root) => Cache::at(root.to_path_buf()),
        None => Cache::from_config().context("failed to open cache")?,
    };
    let stage_root = create_stack_stage_root(opts.dest_root, opts.stack)?;
    let result = run_stack_with_stage(client, &opts, resolved, &cache, &stage_root);
    let _ = fs::remove_dir_all(&stage_root);
    result
}

fn run_stack_with_stage(
    client: &dyn RegistryClient,
    opts: &StackInstallOptions<'_>,
    resolved: &StackResolve,
    cache: &Cache,
    stage_root: &Path,
) -> Result<StackInstallReport> {
    let unpack_parent = stage_root.join("unpacked");
    let stage_target = stage_root.join("target");
    fs::create_dir_all(&unpack_parent)
        .with_context(|| format!("failed to create `{}`", unpack_parent.display()))?;
    fs::create_dir_all(&stage_target)
        .with_context(|| format!("failed to create `{}`", stage_target.display()))?;

    let installed_via = InstallVia::stack(
        resolved.stack.org.clone(),
        resolved.stack.slug.clone(),
        &resolved.manifest_hash,
    );
    let mut staged = Vec::new();
    // `validate_stack_resolution` already rejected duplicate resolved skills.
    for item in &resolved.items {
        let staged_item = stage_stack_item(
            client,
            cache,
            opts,
            item,
            &unpack_parent,
            &stage_target,
            &installed_via,
        )?;
        staged.push(staged_item);
    }

    let receipt = build_stack_receipt(resolved, opts, &staged)?;
    let stack_receipt_path = commit_stack_stage(
        opts.dest_root,
        &mut staged,
        &receipt,
        opts.force,
        &installed_via,
    )?;
    Ok(StackInstallReport {
        stack: resolved.clone(),
        installed: staged
            .into_iter()
            .map(|item| StackInstalledItem {
                skill: item.item.skill,
                version: item.item.version,
                destination: item.final_path,
                receipt_path: item.final_receipt_path,
                overwrote_existing: item.overwrites_existing,
                overlay: item.overlay,
            })
            .collect(),
        stack_receipt_path,
    })
}

struct StagedStackItem {
    item: StackResolvedItem,
    metadata: SkillMetadata,
    stage_path: PathBuf,
    final_path: PathBuf,
    final_receipt_path: PathBuf,
    overwrites_existing: bool,
    overlay: Option<AppliedOverlay>,
}

fn stage_stack_item(
    client: &dyn RegistryClient,
    cache: &Cache,
    opts: &StackInstallOptions<'_>,
    item: &StackResolvedItem,
    unpack_parent: &Path,
    stage_target: &Path,
    installed_via: &InstallVia,
) -> Result<StagedStackItem> {
    let skill_ref = SkillRef::new(opts.org, &item.skill)?.with_version(item.version.clone())?;
    let response = client
        .pull_with_options(
            &skill_ref,
            PullClientOptions {
                allow_yanked: false,
            },
        )
        .with_context(|| format!("download {skill_ref} for stack failed"))?;
    validate_remote_metadata(&response.metadata, &skill_ref)?;
    if response.metadata.hash != item.archive_hash {
        bail!(
            "stack manifest hash for {} expected {} but registry metadata says {}",
            skill_ref,
            item.archive_hash.hex,
            response.metadata.hash.hex
        );
    }
    let actual = PackageHash::sha256_of(&response.archive);
    if actual != item.archive_hash {
        bail!(
            "hash mismatch for {}: expected {} but archive bytes hash to {}",
            skill_ref,
            item.archive_hash.hex,
            actual.hex
        );
    }

    let final_path = opts.dest_root.join(&response.metadata.name);
    let preflight =
        preflight_stack_destination(&final_path, opts.force, &response.metadata, installed_via)?;

    let (unpacked_path, _cache_entry) =
        unpack_and_cache_pull(cache, unpack_parent, &response, actual, || {
            format!("failed to unpack `{skill_ref}`")
        })?;

    let receipt_request = registry_receipt_request(
        &response.metadata,
        opts.registry_url,
        opts.target,
        opts.installed_by.clone(),
        Some(installed_via.clone()),
        preflight.stack_referrers,
    );
    let report = install_skill(InstallOptions {
        source: &unpacked_path,
        dest_root: stage_target,
        name_override: None,
        force: false,
        replace_matching: false,
        receipt: Some(receipt_request),
    })?;
    rewrite_staged_receipt(&report, &final_path)?;

    Ok(StagedStackItem {
        item: item.clone(),
        metadata: response.metadata,
        overlay: report.overlay,
        stage_path: report.destination,
        final_receipt_path: receipt_path(&final_path),
        final_path,
        overwrites_existing: preflight.overwrites_existing,
    })
}

fn install_remote_from_response(
    cache: &Cache,
    stage_parent: &Path,
    response: crate::registry::PullResponse,
    archive_hash: PackageHash,
    opts: RemoteInstallOptions<'_>,
    replace_matching: bool,
) -> Result<RemoteInstallReport> {
    let (unpacked_path, cache_entry) =
        unpack_and_cache_pull(cache, stage_parent, &response, archive_hash, || {
            format!("failed to unpack into `{}`", stage_parent.display())
        })?;

    let receipt = registry_receipt_request(
        &response.metadata,
        opts.registry_url,
        opts.target,
        opts.installed_by,
        None,
        Vec::new(),
    );

    let install = install_skill(InstallOptions {
        source: &unpacked_path,
        dest_root: opts.dest_root,
        name_override: None,
        force: opts.force,
        replace_matching,
        receipt: Some(receipt),
    })?;

    Ok(RemoteInstallReport {
        install,
        metadata: response.metadata,
        cache_entry,
    })
}

/// Unpack a verified registry pull into `unpack_parent`, cross-check the
/// archive's `SKILL.md` name against the registry metadata, and store the
/// archive in the local cache.
fn unpack_and_cache_pull(
    cache: &Cache,
    unpack_parent: &Path,
    response: &crate::registry::PullResponse,
    archive_hash: PackageHash,
    unpack_context: impl FnOnce() -> String,
) -> Result<(PathBuf, CacheEntry)> {
    let unpacked = unpack_verified_bytes(&response.archive, unpack_parent, false, archive_hash)
        .with_context(unpack_context)?;
    validate_unpacked_metadata(&unpacked.manifest, &response.metadata)?;

    let cache_manifest = PackageManifest {
        name: response.metadata.name.clone(),
        description: response.metadata.description.clone(),
        version: response.metadata.version.clone(),
        files: unpacked.manifest.files.clone(),
    };
    let cache_entry = cache.add_archive(
        cache_manifest,
        response.metadata.hash.clone(),
        &response.archive,
    )?;
    Ok((unpacked.out_path, cache_entry))
}

fn registry_receipt_request(
    metadata: &SkillMetadata,
    registry_url: Option<&str>,
    target: &str,
    installed_by: Option<String>,
    installed_via: Option<InstallVia>,
    installed_via_stacks: Vec<InstallVia>,
) -> InstallReceiptRequest {
    InstallReceiptRequest {
        source_type: ReceiptSourceType::Registry,
        source_ref: unversioned_metadata_ref(metadata),
        registry_url: registry_url.map(str::to_string),
        org: Some(metadata.org.clone()),
        version: Some(metadata.version.clone()),
        hash: Some(metadata.hash.clone()),
        content_hash: None,
        target: target.to_string(),
        installed_by,
        installed_via,
        installed_via_stacks,
    }
}

/// Validate a resolved stack against the requested org/stack: the resolved
/// slugs must match and be well-formed, and no skill may resolve twice.
/// `requested` is quoted in the mismatch error so each caller controls how
/// the requested stack is described.
pub(crate) fn validate_stack_resolution(
    resolved: &StackResolve,
    expected_org: &str,
    expected_stack: &str,
    requested: &str,
) -> Result<()> {
    if resolved.stack.org != expected_org || resolved.stack.slug != expected_stack {
        bail!(
            "registry resolved stack `{}/{}` while {requested} was requested",
            resolved.stack.org,
            resolved.stack.slug
        );
    }
    check_slug(&resolved.stack.org).map_err(|reason| {
        anyhow::anyhow!("invalid registry org `{}`: {reason}", resolved.stack.org)
    })?;
    check_slug(&resolved.stack.slug).map_err(|reason| {
        anyhow::anyhow!("invalid registry stack `{}`: {reason}", resolved.stack.slug)
    })?;
    let mut seen = BTreeSet::new();
    for item in &resolved.items {
        check_slug(&item.skill)
            .map_err(|reason| anyhow::anyhow!("invalid stack skill `{}`: {reason}", item.skill))?;
        if !seen.insert(item.skill.clone()) {
            bail!(
                "stack `{}/{}` resolved duplicate skill `{}`",
                resolved.stack.org,
                resolved.stack.slug,
                item.skill
            );
        }
    }
    Ok(())
}

fn rewrite_staged_receipt(report: &InstallReport, final_path: &Path) -> Result<()> {
    let mut receipt = report
        .receipt
        .clone()
        .ok_or_else(|| anyhow::anyhow!("staged stack install did not produce a receipt"))?;
    receipt.installed_path = final_path.to_path_buf();
    write_receipt_to_dir(&report.destination, &receipt)?;
    Ok(())
}

struct StackDestinationPreflight {
    overwrites_existing: bool,
    stack_referrers: Vec<InstallVia>,
}

fn preflight_stack_destination(
    destination: &Path,
    force: bool,
    metadata: &SkillMetadata,
    incoming_stack: &InstallVia,
) -> Result<StackDestinationPreflight> {
    match fs::symlink_metadata(destination) {
        Ok(fs_metadata) => {
            if fs_metadata.file_type().is_symlink() {
                bail!(
                    "`{}` is a symlink; refusing to replace it",
                    destination.display()
                );
            }
            if !fs_metadata.is_dir() {
                bail!("`{}` exists and is not a directory", destination.display());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StackDestinationPreflight {
                overwrites_existing: false,
                stack_referrers: vec![incoming_stack.clone()],
            });
        }
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat `{}`", destination.display()));
        }
    }

    let existing = load_existing_receipt(destination)?;
    let matches_incoming = existing
        .as_ref()
        .is_some_and(|receipt| stack_target_receipt_matches(receipt, metadata));
    if matches_incoming || force {
        let mut refs = existing.as_ref().map(stack_referrers).unwrap_or_default();
        if let Some(receipt) = &existing {
            if matches_incoming && refs.is_empty() && !force {
                bail!(
                    "refusing to adopt existing direct install `{}` at `{}` into stack `{}/{}`; rerun with --force to replace it or remove the direct install first",
                    metadata.skill_ref(),
                    destination.display(),
                    incoming_stack.org,
                    incoming_stack.stack,
                );
            }
            ensure_stack_referrers_can_share_destination(
                destination,
                receipt,
                metadata,
                incoming_stack,
            )?;
        }
        push_unique_stack_ref(&mut refs, incoming_stack.clone());
        return Ok(StackDestinationPreflight {
            overwrites_existing: true,
            stack_referrers: refs,
        });
    }

    let installed = existing
        .as_ref()
        .map(describe_existing)
        .unwrap_or_else(|| "an unmanaged directory (no AgentStack install receipt)".to_string());
    bail!(
        "refusing to overwrite `{}` while installing stack: it currently holds {installed}; rerun with --force to replace it",
        destination.display()
    )
}

fn ensure_stack_referrers_can_share_destination(
    destination: &Path,
    existing: &InstallReceipt,
    metadata: &SkillMetadata,
    incoming_stack: &InstallVia,
) -> Result<()> {
    let refs = stack_referrers(existing)
        .into_iter()
        .filter(|via| !(via.org == incoming_stack.org && via.stack == incoming_stack.stack))
        .collect::<Vec<_>>();
    if refs.is_empty() {
        return Ok(());
    }
    let expected_hash = format_hash(&metadata.hash);
    if existing.version.as_deref() == Some(metadata.version.as_str())
        && existing.hash.as_deref() == Some(expected_hash.as_str())
    {
        return Ok(());
    }
    let owners = refs
        .iter()
        .map(|via| format!("{}/{}", via.org, via.stack))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "refusing to replace shared stack-owned skill `{}` at `{}`: existing version/hash belongs to stack(s) {owners}; align the stacks to the same skill version or install into separate targets",
        metadata.name,
        destination.display()
    )
}

fn stack_target_receipt_matches(receipt: &InstallReceipt, metadata: &SkillMetadata) -> bool {
    receipt.source_type == ReceiptSourceType::Registry
        && receipt.skill_name == metadata.name
        && receipt.org.as_deref() == Some(metadata.org.as_str())
}

fn build_stack_receipt(
    resolved: &StackResolve,
    opts: &StackInstallOptions<'_>,
    staged: &[StagedStackItem],
) -> Result<StackInstallReceipt> {
    Ok(StackInstallReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        kind: "stack".to_string(),
        org: resolved.stack.org.clone(),
        stack: resolved.stack.slug.clone(),
        registry_url: opts.registry_url.map(str::to_string),
        visibility: resolved.stack.visibility,
        team: resolved.stack.team.clone(),
        resolved_at: resolved.resolved_at.clone(),
        manifest_hash: resolved.manifest_hash.clone(),
        target: opts.target.to_string(),
        installed_at: installed_timestamp()?,
        installed_by: opts.installed_by.clone(),
        items: staged
            .iter()
            .map(|item| StackInstallReceiptItem {
                skill: item.item.skill.clone(),
                version_id: item.item.version_id.clone(),
                version: item.item.version.clone(),
                archive_hash: item.item.archive_hash.clone(),
                install_path: item.final_path.clone(),
                installed_receipt_path: item.final_receipt_path.clone(),
            })
            .collect(),
    })
}

fn create_stack_stage_root(dest_root: &Path, stack: &str) -> Result<PathBuf> {
    if dest_root.exists() && !dest_root.is_dir() {
        bail!(
            "target path `{}` exists but is not a directory",
            dest_root.display()
        );
    }
    fs::create_dir_all(dest_root)
        .with_context(|| format!("failed to create `{}`", dest_root.display()))?;
    crate::fs_atomic::create_unique_dir(dest_root, ".agentstack-stack-install-", stack)
        .with_context(|| {
            format!(
                "failed to create a unique stack install staging directory under `{}`",
                dest_root.display()
            )
        })
}

struct StackCommitAction {
    final_path: PathBuf,
    backup_path: Option<PathBuf>,
}

fn commit_stack_stage(
    dest_root: &Path,
    staged: &mut [StagedStackItem],
    receipt: &StackInstallReceipt,
    force: bool,
    incoming_stack: &InstallVia,
) -> Result<PathBuf> {
    let mut actions = Vec::new();
    let commit_result = (|| -> Result<PathBuf> {
        for item in staged {
            let overwrites_existing = revalidate_stack_destination_at_commit(
                &item.final_path,
                item.overwrites_existing,
                force,
                &item.metadata,
                incoming_stack,
            )?;
            item.overwrites_existing = overwrites_existing;
            let backup_path = if overwrites_existing {
                let backup = unique_stack_backup_path(&item.final_path)?;
                fs::rename(&item.final_path, &backup).with_context(|| {
                    format!(
                        "failed to move existing `{}` -> `{}`",
                        item.final_path.display(),
                        backup.display()
                    )
                })?;
                Some(backup)
            } else {
                None
            };
            fs::rename(&item.stage_path, &item.final_path).with_context(|| {
                format!(
                    "failed to move `{}` -> `{}`",
                    item.stage_path.display(),
                    item.final_path.display()
                )
            })?;
            actions.push(StackCommitAction {
                final_path: item.final_path.clone(),
                backup_path,
            });
        }
        write_stack_receipt(dest_root, receipt)
    })();

    match commit_result {
        Ok(path) => {
            for action in actions {
                if let Some(backup) = action.backup_path {
                    fs::remove_dir_all(&backup)
                        .with_context(|| format!("failed to remove `{}`", backup.display()))?;
                }
            }
            Ok(path)
        }
        Err(err) => {
            rollback_stack_commit(actions);
            Err(err)
        }
    }
}

fn revalidate_stack_destination_at_commit(
    destination: &Path,
    initially_existed: bool,
    force: bool,
    metadata: &SkillMetadata,
    incoming_stack: &InstallVia,
) -> Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            if !initially_existed && !force {
                bail!(
                    "destination `{}` appeared during install; refusing to overwrite it. rerun the command after checking the destination.",
                    destination.display()
                );
            }
            Ok(
                preflight_stack_destination(destination, force, metadata, incoming_stack)?
                    .overwrites_existing,
            )
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to stat `{}`", destination.display())),
    }
}

fn rollback_stack_commit(actions: Vec<StackCommitAction>) {
    for action in actions.into_iter().rev() {
        let _ = fs::remove_dir_all(&action.final_path);
        if let Some(backup) = action.backup_path {
            let _ = fs::rename(&backup, &action.final_path);
        }
    }
}

fn unique_stack_backup_path(destination: &Path) -> Result<PathBuf> {
    crate::fs_atomic::reserve_sibling_path(destination, ".agentstack-stack-backup-").with_context(
        || {
            format!(
                "failed to create a unique stack backup path next to `{}`",
                destination.display()
            )
        },
    )
}

fn parse_source(raw: &str, typed_name: Option<&str>, org: Option<&str>) -> Result<InstallSource> {
    match (raw, typed_name) {
        ("stack", Some(stack)) => {
            check_slug(stack)
                .map_err(|reason| anyhow::anyhow!("invalid stack `{stack}`: {reason}"))?;
            let raw = match org {
                Some(org) => {
                    let org = require_typed_org(Some(org))?;
                    format!("{org}/{stack}")
                }
                None => stack.to_string(),
            };
            return Ok(InstallSource::Stack { raw });
        }
        (_, Some(_)) => {
            bail!("unknown internal install source kind `{raw}` (expected `stack`)");
        }
        (_, None) => {}
    }
    if org.is_some() {
        bail!(
            "--org is only valid for stack install routing; use `agentstack skill install <org/skill>` or `agentstack stack install <org/stack>`"
        );
    }

    let path = PathBuf::from(raw);
    if path.is_dir() && path.join("SKILL.md").is_file() {
        return Ok(InstallSource::Local(path));
    }
    if SkillRef::parse(raw).is_ok() {
        return Ok(InstallSource::Remote {
            raw: raw.to_string(),
        });
    }
    if looks_like_local_path(raw) {
        return Ok(InstallSource::Local(path));
    }
    if refs::SkillRefInput::parse(raw)?.requires_org_resolution() {
        return Ok(InstallSource::Remote {
            raw: raw.to_string(),
        });
    }
    Ok(InstallSource::Local(path))
}

fn looks_like_local_path(raw: &str) -> bool {
    raw.starts_with('.')
        || raw.starts_with('/')
        || raw.starts_with('~')
        || raw.contains(std::path::MAIN_SEPARATOR)
}

fn require_typed_org(org: Option<&str>) -> Result<&str> {
    let org = org.ok_or_else(|| anyhow::anyhow!("internal stack install routing requires org"))?;
    check_slug(org).map_err(|reason| anyhow::anyhow!("invalid --org `{org}`: {reason}"))?;
    Ok(org)
}

fn resolve_install_target(ctx: &Ctx, target: Option<&str>) -> Result<ResolvedInstallTarget> {
    match target {
        Some(target) => resolve_target(InstallTarget::parse(target)?),
        None => select_configured_target(ctx).map(|resolved| ResolvedInstallTarget {
            target: resolved.target,
            path: resolved.path,
            source: resolved.source,
            pending_registration: false,
        }),
    }
}

/// Resolve an explicit `--target <name>`. If no override is configured, fall
/// back to the platform default path but defer registration until a successful
/// install so denied registry installs do not mutate local state.
fn resolve_target(target: InstallTarget) -> Result<ResolvedInstallTarget> {
    let store = ConfigStore::load().context("failed to load config")?;
    if store.target_override(target.as_str()).is_some() {
        let resolved = TargetResolver::new(&store).resolve(target)?;
        return Ok(ResolvedInstallTarget {
            target: resolved.target,
            path: resolved.path,
            source: resolved.source,
            pending_registration: false,
        });
    }

    let path = default_target_path(target).with_context(|| {
        format!(
            "no path configured for target `{}` and no platform default could be \
             derived (set one with `agentstack target set {} --path <path>`)",
            target.as_str(),
            target.as_str(),
        )
    })?;
    if matches!(target, InstallTarget::ClaudeCode | InstallTarget::Codex) {
        return Err(CliError::new(
            "target_not_configured",
            format!(
                "target `{}` is not configured; run `agentstack target setup {} --yes` to accept the default path `{}` or `agentstack target set {} --path <absolute-path>`",
                target.as_str(),
                target.as_str(),
                path.display(),
                target.as_str(),
            ),
        )
        .resource(target.as_str())
        .action("configure_target")
        .next_command(format!("agentstack target setup {} --yes", target.as_str()))
        .into());
    }
    Ok(ResolvedInstallTarget {
        target,
        path,
        source: TargetSource::Override,
        pending_registration: true,
    })
}

fn commit_pending_target_registration(ctx: &Ctx, resolved: &ResolvedInstallTarget) -> Result<()> {
    if !resolved.pending_registration {
        return Ok(());
    }
    let mut store = ConfigStore::load().context("failed to load config")?;
    if store.target_override(resolved.target.as_str()).is_some() {
        return Ok(());
    }
    setup::auto_register(&mut store, resolved.target, &resolved.path)?;
    if !ctx.json && !ctx.quiet {
        let _ = writeln!(
            io::stderr(),
            "note: registered target `{}` -> {}",
            resolved.target.as_str(),
            resolved.path.display()
        );
    }
    Ok(())
}

fn select_configured_target(ctx: &Ctx) -> Result<crate::targets::ResolvedTarget> {
    let store = ConfigStore::load().context("failed to load config")?;
    let candidates = configured_usable_targets(&store);
    match candidates.len() {
        0 => bail!(
            "no configured usable install target; run `agentstack target setup <target> --path <absolute-path>` or `agentstack target set <target> --path <absolute-path>`"
        ),
        1 => {
            let resolved = candidates.into_iter().next().unwrap();
            if !ctx.json && !ctx.quiet {
                ctx.say(configured_target_message(&resolved));
            }
            Ok(resolved)
        }
        _ if ctx.can_prompt() => prompt_for_target(candidates),
        _ => bail!(
            "multiple configured usable install targets ({}); rerun with `--target <target>`",
            candidates
                .iter()
                .map(|c| c.target.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn configured_usable_targets(store: &ConfigStore) -> Vec<crate::targets::ResolvedTarget> {
    let resolver = TargetResolver::new(store);
    InstallTarget::ALL
        .iter()
        .filter_map(|target| {
            let detection = resolver.detect(*target);
            if !(detection.configured && detection.usable) {
                return None;
            }
            Some(crate::targets::ResolvedTarget {
                target: *target,
                path: detection.path?,
                source: TargetSource::Override,
            })
        })
        .collect()
}

fn prompt_for_target(
    candidates: Vec<crate::targets::ResolvedTarget>,
) -> Result<crate::targets::ResolvedTarget> {
    eprintln!("multiple configured install targets are available:");
    for (idx, candidate) in candidates.iter().enumerate() {
        eprintln!(
            "  {}) {:<11} {}",
            idx + 1,
            candidate.target.as_str(),
            candidate.path.display()
        );
    }
    loop {
        eprint!("select target [1-{}]: ", candidates.len());
        io::stderr().flush().context("failed to flush prompt")?;
        let mut input = String::new();
        let read = io::stdin()
            .read_line(&mut input)
            .context("failed to read target selection")?;
        if read == 0 {
            bail!("no target selected; rerun with `--target <target>`");
        }
        let input = input.trim();
        if let Ok(idx) = input.parse::<usize>()
            && (1..=candidates.len()).contains(&idx)
        {
            return Ok(candidates[idx - 1].clone());
        }
        if let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.target.as_str() == input)
        {
            return Ok(candidate.clone());
        }
        eprintln!("{}", invalid_target_selection_message(candidates.len()));
    }
}

fn configured_target_message(resolved: &crate::targets::ResolvedTarget) -> String {
    format!(
        "using configured target `{}` ({})",
        resolved.target.as_str(),
        resolved.path.display()
    )
}

fn invalid_target_selection_message(candidate_count: usize) -> String {
    format!("enter a number from 1 to {candidate_count} or a target name.")
}

pub(crate) fn create_remote_stage_parent(cache: &Cache, skill_name: &str) -> Result<PathBuf> {
    let root = cache.root().join("staging").join("install");
    fs::create_dir_all(&root).with_context(|| format!("failed to create `{}`", root.display()))?;
    crate::fs_atomic::create_unique_dir(&root, "", skill_name).with_context(|| {
        format!(
            "failed to create a unique remote install staging directory under `{}`",
            root.display()
        )
    })
}

fn validate_remote_metadata(metadata: &SkillMetadata, requested: &SkillRef) -> Result<()> {
    check_slug(&metadata.org)
        .map_err(|reason| anyhow::anyhow!("invalid registry org `{}`: {reason}", metadata.org))?;
    check_slug(&metadata.name).map_err(|reason| {
        anyhow::anyhow!("invalid registry skill name `{}`: {reason}", metadata.name)
    })?;
    check_version(&metadata.version)
        .with_context(|| format!("invalid registry version `{}`", metadata.version))?;

    if metadata.org != requested.org || metadata.name != requested.name {
        bail!(
            "registry returned metadata for `{}/{}` while `{}` was requested",
            metadata.org,
            metadata.name,
            requested,
        );
    }
    if let Some(version) = &requested.version
        && metadata.version != *version
    {
        bail!(
            "registry returned version `{}` while pinned version `{version}` was requested",
            metadata.version,
        );
    }
    Ok(())
}

fn validate_unpacked_metadata(manifest: &PackageManifest, metadata: &SkillMetadata) -> Result<()> {
    if manifest.name != metadata.name {
        bail!(
            "registry metadata name `{}` does not match archive SKILL.md name `{}`",
            metadata.name,
            manifest.name,
        );
    }
    Ok(())
}

fn local_source_ref(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn unversioned_metadata_ref(metadata: &SkillMetadata) -> String {
    format!("{}/{}", metadata.org, metadata.name)
}

/// Warn when a registry skill carries platform tags and none of them matches
/// the install target's platform. Targets without a platform (`local`) and
/// untagged skills produce no warning.
fn platform_mismatch_warning(platform_tags: &[String], target: InstallTarget) -> Option<String> {
    let platform = target.platform()?;
    if platform_tags.is_empty() || platform_tags.iter().any(|tag| tag == platform) {
        return None;
    }
    Some(format!(
        "skill is tagged for {}; installing to {} target",
        platform_tags.join(", "),
        target.as_str()
    ))
}

fn warn_platform_mismatch(ctx: &Ctx, warning: Option<&str>) {
    if let Some(warning) = warning
        && !ctx.json
    {
        ctx.warn(format!("warning: {warning}"));
    }
}

fn warn_if_foreign_overwrite(ctx: &Ctx, report: &InstallReport) {
    if report.replaced_foreign && !ctx.json {
        ctx.warn(format!(
            "warning: replaced existing contents at `{}` because --force was set",
            report.destination.display()
        ));
    }
}

fn render_success(ctx: &Ctx, install: &RenderInstall<'_>) -> Result<()> {
    if ctx.json {
        let payload = InstallJson {
            kind: "skill_install",
            operation: "install",
            resource: &install.report.installed_as,
            name: &install.report.manifest_name,
            installed_as: &install.report.installed_as,
            target: install.target,
            target_source: install.target_source.as_str(),
            destination: &install.report.destination,
            source_type: install.source_type.as_str(),
            source_ref: &install.source_ref,
            registry_url: install.registry_url.as_deref(),
            org: install.org.as_deref(),
            version: install.version.as_deref(),
            hash: install.hash.as_deref(),
            hash_kind: install
                .hash
                .as_ref()
                .map(|_| install.source_type.hash_kind()),
            receipt: install.report.receipt_path.as_ref(),
            cache_package: install.cache_package_path.as_ref(),
            files: install.report.files_copied,
            overwrote: install.report.overwrote_existing,
            overlay: install.report.overlay.as_ref(),
            platform_warning: install.platform_warning.as_deref(),
            warnings: &install.report.warnings,
            next_commands: skill_install_next_commands(install),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say_always(format!("installed skill {}", install.installed_label));
    if ctx.quiet {
        return Ok(());
    }

    ctx.say("");
    ctx.say(format!("target: {}", install.target));
    ctx.say(format!(
        "destination: {}",
        install.report.destination.display()
    ));
    if install.report.overwrote_existing {
        ctx.say("overwrote: yes (--force)");
    }
    if let Some(overlay) = &install.report.overlay {
        ctx.say(format!("applied platform overlay: {}", overlay.describe()));
    }

    ctx.say("");
    match install.source_type {
        ReceiptSourceType::Registry => {
            if let Some(url) = &install.registry_url {
                ctx.say(format!("registry: {url}"));
            }
            ctx.say(format!("source: {}", install.source_ref));
            if let Some(version) = &install.version {
                ctx.say(format!("version: {version}"));
            }
        }
        ReceiptSourceType::Local => {
            ctx.say(format!("source: local {}", install.source_ref));
        }
    }
    if let Some(hash) = &install.hash {
        ctx.say(format!("{}: {hash}", install.source_type.hash_label()));
    }
    if let Some(receipt) = &install.report.receipt_path {
        ctx.say(format!("receipt: {}", receipt.display()));
    }

    if !install.report.warnings.is_empty() {
        ctx.say("");
        ctx.say("warnings:");
        for warning in &install.report.warnings {
            ctx.say(format!("  - {warning}"));
        }
    }

    ctx.say("");
    ctx.say("next:");
    for command in skill_install_next_commands(install) {
        ctx.say(format!("  {command}"));
    }
    Ok(())
}

fn skill_install_next_commands(install: &RenderInstall<'_>) -> Vec<String> {
    let mut commands = vec![
        format!(
            "agentstack skill show {} --target {}",
            install.report.installed_as, install.target
        ),
        format!(
            "agentstack skill validate {}",
            install.report.destination.display()
        ),
    ];
    if install.source_type == ReceiptSourceType::Registry {
        commands.push(format!(
            "agentstack skill update {} --target {} --check",
            install.report.installed_as, install.target
        ));
    }
    commands
}

#[derive(Serialize)]
struct StackInstallJson<'a> {
    kind: &'static str,
    operation: &'static str,
    resource: String,
    org: &'a str,
    stack: &'a str,
    target: &'a str,
    target_source: &'static str,
    manifest_hash: &'a PackageHash,
    resolved_at: &'a str,
    stack_receipt: &'a Path,
    items: &'a [StackInstalledItem],
    next_commands: Vec<String>,
}

fn render_stack_success(
    ctx: &Ctx,
    report: &StackInstallReport,
    target: &str,
    target_source: TargetSource,
) -> Result<()> {
    if ctx.json {
        let payload = StackInstallJson {
            kind: "stack_install",
            operation: "install",
            resource: format!("{}/{}", report.stack.stack.org, report.stack.stack.slug),
            org: &report.stack.stack.org,
            stack: &report.stack.stack.slug,
            target,
            target_source: target_source.as_str(),
            manifest_hash: &report.stack.manifest_hash,
            resolved_at: &report.stack.resolved_at,
            stack_receipt: &report.stack_receipt_path,
            items: &report.installed,
            next_commands: stack_install_next_commands(report, target),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let updated_existing =
        !report.installed.is_empty() && report.installed.iter().all(|item| item.overwrote_existing);
    let verb = if updated_existing {
        "refreshed existing stack"
    } else {
        "installed stack"
    };
    ctx.say_always(format!(
        "{verb} {}/{}",
        report.stack.stack.org, report.stack.stack.slug
    ));
    if ctx.quiet {
        return Ok(());
    }

    ctx.say("");
    ctx.say(format!("target: {target}"));
    ctx.say(format!("items: {}", report.installed.len()));
    ctx.say(format!("receipt: {}", report.stack_receipt_path.display()));
    ctx.say(format!(
        "source: {}/{}",
        report.stack.stack.org, report.stack.stack.slug
    ));
    ctx.say(format!(
        "manifest hash: {}",
        crate::receipt::format_hash(&report.stack.manifest_hash)
    ));
    ctx.say(format!("resolved: {}", report.stack.resolved_at));

    ctx.say("");
    ctx.say("installed skills:");
    for item in &report.installed {
        let overwrite = if item.overwrote_existing {
            " (refreshed)"
        } else {
            ""
        };
        ctx.say(format!(
            "  - {}@{} -> {}{}",
            item.skill,
            item.version,
            item.destination.display(),
            overwrite
        ));
        if let Some(overlay) = &item.overlay {
            ctx.say(format!(
                "    applied platform overlay: {}",
                overlay.describe()
            ));
        }
    }
    ctx.say("");
    ctx.say("next:");
    for command in stack_install_next_commands(report, target) {
        ctx.say(format!("  {command}"));
    }
    Ok(())
}

fn stack_install_next_commands(report: &StackInstallReport, target: &str) -> Vec<String> {
    vec![
        format!(
            "agentstack stack show {}/{} --target {}",
            report.stack.stack.org, report.stack.stack.slug, target
        ),
        format!("agentstack install doctor --target {target}"),
        format!(
            "agentstack stack update {}/{} --target {} --check",
            report.stack.stack.org, report.stack.stack.slug, target
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_mismatch_warning_only_fires_for_unmatched_tags() {
        let tags = vec!["codex".to_string()];
        assert_eq!(
            platform_mismatch_warning(&tags, InstallTarget::ClaudeCode).as_deref(),
            Some("skill is tagged for codex; installing to claude-code target")
        );
        assert!(platform_mismatch_warning(&tags, InstallTarget::Codex).is_none());
        assert!(platform_mismatch_warning(&tags, InstallTarget::RepoCodex).is_none());
        assert!(
            platform_mismatch_warning(&tags, InstallTarget::Local).is_none(),
            "local target has no platform to mismatch"
        );
        assert!(
            platform_mismatch_warning(&[], InstallTarget::ClaudeCode).is_none(),
            "untagged skills install anywhere without warning"
        );
    }

    #[test]
    fn target_selection_copy_stays_quiet() {
        let resolved = crate::targets::ResolvedTarget {
            target: InstallTarget::Local,
            path: PathBuf::from("/tmp/agentstack-target"),
            source: TargetSource::Override,
        };

        assert_eq!(
            configured_target_message(&resolved),
            "using configured target `local` (/tmp/agentstack-target)"
        );
        assert_eq!(
            invalid_target_selection_message(4),
            "enter a number from 1 to 4 or a target name."
        );
    }

    #[test]
    fn stack_commit_recheck_refuses_destination_that_appeared_without_force() {
        let scratch = crate::fs_atomic::create_unique_dir(
            &std::env::temp_dir(),
            "",
            "agentstack-stack-commit-recheck",
        )
        .unwrap();
        let staging = scratch.join("staging");
        let destination = scratch.join("skill");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("SKILL.md"), b"new").unwrap();
        fs::create_dir_all(&destination).unwrap();

        let metadata = SkillMetadata {
            name: "skill".to_string(),
            description: "Use when skill tasks come up".to_string(),
            org: "acme".to_string(),
            owner_email: None,
            team: None,
            visibility: crate::registry::Visibility::Org,
            version: "1".to_string(),
            hash: PackageHash::sha256_of(b"skill"),
            platform_tags: Vec::new(),
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
        let incoming_stack =
            InstallVia::stack("acme".to_string(), "default".to_string(), &metadata.hash);
        let mut staged = vec![StagedStackItem {
            item: StackResolvedItem {
                skill: "skill".to_string(),
                version_id: "1".to_string(),
                version: "1".to_string(),
                archive_hash: metadata.hash.clone(),
                download: crate::registry::StackDownloadRoute {
                    method: "GET".to_string(),
                    url: "mock://registry/skill".to_string(),
                },
                version_policy: crate::registry::VersionPolicy::Current,
            },
            metadata,
            stage_path: staging.clone(),
            final_path: destination.clone(),
            final_receipt_path: receipt_path(&destination),
            overwrites_existing: false,
            overlay: None,
        }];
        let receipt = StackInstallReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            kind: "stack".to_string(),
            org: "acme".to_string(),
            stack: "default".to_string(),
            registry_url: None,
            visibility: crate::registry::Visibility::Org,
            team: None,
            resolved_at: "now".to_string(),
            manifest_hash: PackageHash::sha256_of(b"manifest"),
            target: "local".to_string(),
            installed_at: "now".to_string(),
            installed_by: None,
            items: Vec::new(),
        };

        let err = commit_stack_stage(&scratch, &mut staged, &receipt, false, &incoming_stack)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("appeared during install"), "got: {msg}");
        assert!(destination.is_dir());
        assert!(staging.join("SKILL.md").is_file());

        let _ = fs::remove_dir_all(&scratch);
    }
}
