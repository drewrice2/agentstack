//! Registry export implementation for `skill export` and `stack export`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::client::{configured_client, registry_context};
use super::install::validate_stack_resolution;
use super::refs;
use crate::error::CliError;
use crate::output::Ctx;
use crate::package::{PackageHash, unpack_verified_bytes};
use crate::registry::{
    PullClientOptions, PullResponse, RegistryClient, SkillMetadata, StackResolve,
    StackResolvedItem, Visibility,
};
use crate::skill_ref::SkillRef;

pub struct Args {
    pub source: String,
    pub source_name: Option<String>,
    pub org: Option<String>,
    pub team: Option<String>,
    pub out: Option<PathBuf>,
    pub force: bool,
    pub dry_run: bool,
    pub allow_yanked: bool,
}

pub struct ExportOptions<'a> {
    pub skill_ref: &'a SkillRef,
    pub out: Option<&'a Path>,
    pub force: bool,
    pub json: bool,
    pub quiet: bool,
    pub dry_run: bool,
    pub allow_yanked: bool,
}

enum ExportSource {
    /// Raw skill ref, either `org/skill[@version]` or `skill[@version]`.
    Skill(String),
    /// Raw stack ref, either `org/stack` or `stack`.
    Stack(String),
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let source = parse_export_source(
        &args.source,
        args.source_name.as_deref(),
        args.org.as_deref(),
    )?;
    if matches!(source, ExportSource::Stack(_)) && args.allow_yanked {
        bail!("--allow-yanked is only valid for skill exports");
    }
    if let ExportSource::Skill(raw) = &source {
        let input = refs::validate_skill_ref_input_with_team(raw, args.team.as_deref())?;
        if args.allow_yanked && input.version().is_none() {
            match input {
                refs::SkillRefInput::Qualified(_) => bail!(
                    "--allow-yanked requires an explicit pinned ref such as `org/skill@version`"
                ),
                refs::SkillRefInput::Relative { .. } => bail!(
                    "--allow-yanked requires an explicit pinned ref such as `skill@version` or `org/skill@version`"
                ),
            }
        }
    }
    let configured = configured_client()?;
    ctx.verbose(format!("registry: {}", configured.url));

    match source {
        ExportSource::Skill(raw) => {
            let skill_ref = refs::resolve_skill_ref_with_team(
                ctx,
                &configured.client,
                &raw,
                args.team.as_deref(),
            )?;
            run_with_client(
                &configured.client,
                Some(&configured.url),
                ExportOptions {
                    skill_ref: &skill_ref,
                    out: args.out.as_deref(),
                    force: args.force,
                    json: ctx.json,
                    quiet: ctx.quiet,
                    dry_run: args.dry_run,
                    allow_yanked: args.allow_yanked,
                },
            )
        }
        ExportSource::Stack(raw) => {
            let (org, stack) = refs::resolve_stack_ref_with_team(
                ctx,
                &configured.client,
                &raw,
                None,
                args.team.as_deref(),
            )?;
            run_stack_with_client(
                &configured.client,
                Some(&configured.url),
                StackExportOptions {
                    org: &org,
                    stack: &stack,
                    out: args.out.as_deref(),
                    force: args.force,
                    json: ctx.json,
                    quiet: ctx.quiet,
                    dry_run: args.dry_run,
                },
            )
        }
    }
}

fn parse_export_source(
    source: &str,
    typed_name: Option<&str>,
    org: Option<&str>,
) -> Result<ExportSource> {
    match (source, typed_name) {
        ("stack", Some(stack)) => {
            crate::skill::check_slug(stack)
                .map_err(|reason| anyhow::anyhow!("invalid stack `{stack}`: {reason}"))?;
            return match org {
                Some(org) => {
                    crate::skill::check_slug(org)
                        .map_err(|reason| anyhow::anyhow!("invalid --org `{org}`: {reason}"))?;
                    Ok(ExportSource::Stack(format!("{org}/{stack}")))
                }
                None => Ok(ExportSource::Stack(stack.to_string())),
            };
        }
        (_, Some(_)) => {
            if source.parse::<SkillRef>().is_ok() {
                bail!(
                    "unexpected positional argument after `{source}`; use `--out <DIR>` to choose an export directory"
                );
            }
            bail!("unknown internal export source kind `{source}` (expected `stack`)");
        }
        (_, None) => {}
    }
    if org.is_some() {
        bail!(
            "--org is only valid for stack export routing; use `agentstack skill export <org/skill>` or `agentstack stack export <org/stack>`"
        );
    }

    if source.parse::<SkillRef>().is_err() {
        refs::parse_relative_skill_ref(source)?;
    }
    Ok(ExportSource::Skill(source.to_string()))
}

pub fn run_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    opts: ExportOptions<'_>,
) -> Result<()> {
    if opts.allow_yanked && opts.skill_ref.version.is_none() {
        bail!("--allow-yanked requires an explicit pinned ref such as `org/skill@version`");
    }
    let response: PullResponse = client
        .pull_with_options(
            opts.skill_ref,
            PullClientOptions {
                allow_yanked: opts.allow_yanked,
            },
        )
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

    let dest_owned;
    let destination_parent: &Path = match opts.out {
        Some(p) => p,
        None => {
            dest_owned = PathBuf::from(".");
            &dest_owned
        }
    };

    if opts.dry_run {
        let projected = destination_parent.join(&response.metadata.name);
        check_destination_clear(&projected, opts.force)?;
        if opts.json {
            println!("{}", render_dry_run_json(&response, &projected)?);
        } else {
            render_dry_run_human(&response, &projected, opts.quiet);
        }
        return Ok(());
    }

    check_destination_clear(
        &destination_parent.join(&response.metadata.name),
        opts.force,
    )?;
    let unpacked = unpack_verified_bytes(&response.archive, destination_parent, opts.force, actual)
        .with_context(|| format!("failed to unpack into `{}`", destination_parent.display()))?;

    if opts.json {
        println!("{}", render_json(&response, &unpacked.out_path)?);
    } else {
        render_human(&response, &unpacked.out_path, opts.quiet);
    }
    Ok(())
}

struct StackExportOptions<'a> {
    org: &'a str,
    stack: &'a str,
    out: Option<&'a Path>,
    force: bool,
    json: bool,
    quiet: bool,
    dry_run: bool,
}

struct DownloadedStackItem {
    item: StackResolvedItem,
    response: PullResponse,
    archive_hash: PackageHash,
}

fn run_stack_with_client(
    client: &dyn RegistryClient,
    registry_url: Option<&str>,
    opts: StackExportOptions<'_>,
) -> Result<()> {
    let resolved = client
        .resolve_stack(opts.org, opts.stack)
        .with_context(|| registry_context(registry_url, "resolve stack from", "resolve stack"))?;
    validate_stack_resolution(
        &resolved,
        opts.org,
        opts.stack,
        &format!("`{}/{}`", opts.org, opts.stack),
    )?;
    let downloaded = download_stack_items(client, &resolved, registry_url)?;

    let dest_owned;
    let destination_parent: &Path = match opts.out {
        Some(p) => p,
        None => {
            dest_owned = PathBuf::from(".");
            &dest_owned
        }
    };
    for item in &downloaded {
        check_destination_clear(
            &destination_parent.join(&item.response.metadata.name),
            opts.force,
        )?;
    }

    if opts.dry_run {
        if opts.json {
            println!(
                "{}",
                render_stack_dry_run_json(&resolved, &downloaded, destination_parent)?
            );
        } else {
            render_stack_dry_run_human(&resolved, &downloaded, destination_parent, opts.quiet);
        }
        return Ok(());
    }

    let mut destinations = Vec::new();
    for item in downloaded {
        let unpacked = unpack_verified_bytes(
            &item.response.archive,
            destination_parent,
            opts.force,
            item.archive_hash,
        )
        .with_context(|| {
            format!(
                "failed to unpack `{}/{}@{}`",
                item.response.metadata.org,
                item.response.metadata.name,
                item.response.metadata.version
            )
        })?;
        destinations.push(StackExportDestination {
            skill: item.response.metadata.name,
            version: item.response.metadata.version,
            destination: unpacked.out_path,
        });
    }

    if opts.json {
        println!(
            "{}",
            render_stack_json(&resolved, &destinations, destination_parent)?
        );
    } else {
        render_stack_human(&resolved, &destinations, destination_parent, opts.quiet);
    }
    Ok(())
}

fn download_stack_items(
    client: &dyn RegistryClient,
    resolved: &StackResolve,
    registry_url: Option<&str>,
) -> Result<Vec<DownloadedStackItem>> {
    let mut out = Vec::new();
    for item in &resolved.items {
        let skill_ref =
            SkillRef::new(&resolved.stack.org, &item.skill)?.with_version(item.version.clone())?;
        let response = client
            .pull_with_options(
                &skill_ref,
                PullClientOptions {
                    allow_yanked: false,
                },
            )
            .with_context(|| {
                registry_context(
                    registry_url,
                    &format!("download {skill_ref} from"),
                    &format!("registry download {skill_ref}"),
                )
            })?;
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
        out.push(DownloadedStackItem {
            item: item.clone(),
            response,
            archive_hash: actual,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
struct StackExportDestination {
    skill: String,
    version: String,
    destination: PathBuf,
}

#[derive(Serialize)]
struct StackExportJson<'a> {
    stack: &'a crate::registry::StackResolveHeader,
    manifest_hash: &'a PackageHash,
    destination_parent: &'a Path,
    items: &'a [StackExportDestination],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    next_command_templates: Vec<String>,
}

#[derive(Serialize)]
struct StackExportDryRunJson<'a> {
    would_export: bool,
    stack: &'a crate::registry::StackResolveHeader,
    manifest_hash: &'a PackageHash,
    destination_parent: &'a Path,
    items: Vec<StackExportDestination>,
}

fn render_stack_json(
    resolved: &StackResolve,
    destinations: &[StackExportDestination],
    destination_parent: &Path,
) -> Result<String> {
    Ok(serde_json::to_string_pretty(&StackExportJson {
        stack: &resolved.stack,
        manifest_hash: &resolved.manifest_hash,
        destination_parent,
        items: destinations,
        next_commands: Vec::new(),
        next_command_templates: stack_export_next_command_templates(resolved),
    })?)
}

fn render_stack_dry_run_json(
    resolved: &StackResolve,
    downloaded: &[DownloadedStackItem],
    destination_parent: &Path,
) -> Result<String> {
    let items = downloaded
        .iter()
        .map(|item| StackExportDestination {
            skill: item.item.skill.clone(),
            version: item.item.version.clone(),
            destination: destination_parent.join(&item.item.skill),
        })
        .collect();
    Ok(serde_json::to_string_pretty(&StackExportDryRunJson {
        would_export: true,
        stack: &resolved.stack,
        manifest_hash: &resolved.manifest_hash,
        destination_parent,
        items,
    })?)
}

fn render_stack_human(
    resolved: &StackResolve,
    destinations: &[StackExportDestination],
    destination_parent: &Path,
    quiet: bool,
) {
    println!(
        "exported stack {}/{}",
        resolved.stack.org, resolved.stack.slug
    );
    if quiet {
        return;
    }
    println!("  mode:        unmanaged export (no install receipts)");
    println!("  destination: {}", destination_parent.display());
    println!("  manifest:    {}", resolved.manifest_hash.hex);
    for item in destinations {
        println!(
            "  - {}@{} -> {}",
            item.skill,
            item.version,
            item.destination.display()
        );
    }
    println!();
    println!("next:");
    println!(
        "  agentstack stack install {}/{} --target <target>",
        resolved.stack.org, resolved.stack.slug
    );
}

fn render_stack_dry_run_human(
    resolved: &StackResolve,
    downloaded: &[DownloadedStackItem],
    destination_parent: &Path,
    quiet: bool,
) {
    println!(
        "would export stack {}/{}",
        resolved.stack.org, resolved.stack.slug
    );
    if quiet {
        return;
    }
    println!("  mode:        unmanaged export (no install receipts)");
    println!("  destination: {}", destination_parent.display());
    println!("  manifest:    {}", resolved.manifest_hash.hex);
    for item in downloaded {
        println!(
            "  - {}@{} -> {}",
            item.item.skill,
            item.item.version,
            destination_parent.join(&item.item.skill).display()
        );
    }
    println!();
    println!("archives verified (hashes match manifest).");
    println!("next: rerun without --dry-run to unpack");
}

/// Pre-check that the projected `<out>/<skill-name>` directory is either
/// absent, empty, or being replaced with `--force`. Mirrors the conflict
/// rule that unpacking enforces, so `--dry-run` reports the same
/// failure that a real export would hit.
fn check_destination_clear(destination: &Path, force: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(CliError::new(
                    "destination_conflict",
                    format!(
                        "`{}` is a symlink; refusing to unpack into it",
                        destination.display()
                    ),
                )
                .resource(destination.display().to_string())
                .action("export")
                .next_command("rerun the same command with --force only if the path is safe")
                .into());
            }
            if !metadata.is_dir() {
                return Err(CliError::new(
                    "destination_conflict",
                    format!("`{}` exists and is not a directory", destination.display()),
                )
                .resource(destination.display().to_string())
                .action("export")
                .next_command("choose a different --out directory")
                .into());
            }
            let has_entries = fs::read_dir(destination)
                .with_context(|| format!("failed to read `{}`", destination.display()))?
                .next()
                .is_some();
            if has_entries {
                return Err(CliError::new(
                    "destination_exists",
                    format!(
                        "refusing to overwrite `{}` (rerun with --force to replace)",
                        destination.display()
                    ),
                )
                .resource(destination.display().to_string())
                .action("export")
                .next_command("rerun the same command with --force")
                .into());
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to stat `{}`", destination.display())),
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    skill_ref: String,
    metadata: &'a SkillMetadata,
    destination: &'a Path,
    next_commands: Vec<String>,
    next_command_templates: Vec<String>,
}

#[derive(Serialize)]
struct DryRunJsonOutput<'a> {
    would_export: bool,
    skill_ref: String,
    metadata: &'a SkillMetadata,
    destination: &'a Path,
}

fn render_json(response: &PullResponse, destination: &Path) -> Result<String> {
    let out = JsonOutput {
        skill_ref: response.metadata.skill_ref(),
        metadata: &response.metadata,
        destination,
        next_commands: skill_export_next_commands(destination),
        next_command_templates: skill_export_next_command_templates(destination),
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_dry_run_json(response: &PullResponse, destination: &Path) -> Result<String> {
    let out = DryRunJsonOutput {
        would_export: true,
        skill_ref: response.metadata.skill_ref(),
        metadata: &response.metadata,
        destination,
    };
    Ok(serde_json::to_string_pretty(&out)?)
}

fn render_human(response: &PullResponse, destination: &Path, quiet: bool) {
    println!("exported {}", response.metadata.skill_ref());
    if !quiet {
        print_summary_lines(response, destination);
    }
    if quiet {
        return;
    }
    println!();
    println!("next:");
    println!("  agentstack skill validate {}", destination.display());
    println!(
        "  agentstack skill install {} --target <target>",
        destination.display()
    );
}

fn render_dry_run_human(response: &PullResponse, destination: &Path, quiet: bool) {
    println!("would export {}", response.metadata.skill_ref());
    if quiet {
        return;
    }
    print_summary_lines(response, destination);
    println!();
    println!("archive verified (hash matches metadata).");
    println!("next: rerun without --dry-run to unpack");
}

fn skill_export_next_commands(destination: &Path) -> Vec<String> {
    vec![format!(
        "agentstack skill validate {}",
        destination.display()
    )]
}

fn skill_export_next_command_templates(destination: &Path) -> Vec<String> {
    vec![format!(
        "agentstack skill install {} --target <target>",
        destination.display()
    )]
}

fn stack_export_next_command_templates(resolved: &StackResolve) -> Vec<String> {
    vec![format!(
        "agentstack stack install {}/{} --target <target>",
        resolved.stack.org, resolved.stack.slug
    )]
}

fn print_summary_lines(response: &PullResponse, destination: &Path) {
    println!("  destination: {}", destination.display());
    println!("  version:     {}", response.metadata.version);
    println!("  hash:        {}", response.metadata.hash.hex);
    println!(
        "  visibility:  {} {}",
        response.metadata.visibility,
        visibility_hint(response.metadata.visibility)
    );
    if let Some(team) = &response.metadata.team {
        println!("  team:        {team}");
    }
}

fn visibility_hint(v: Visibility) -> &'static str {
    match v {
        Visibility::Org => "(any member of the owning org can read)",
        Visibility::Private => "(only the owner and admins can read)",
        Visibility::Team => "(members of the owning team and admins can read)",
    }
}
