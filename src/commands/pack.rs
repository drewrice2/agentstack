use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::cache::Cache;
use crate::output::Ctx;
use crate::package::{PACKAGE_EXTENSION, SkillPackage, build_skill_package};
use crate::skill::{LintConfig, lint_skill, validate_skill};

pub struct Args {
    pub path: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub force: bool,
    pub no_cache: bool,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let source = args.path.unwrap_or_else(|| PathBuf::from("."));

    let built = build_skill_package(&source)
        .with_context(|| format!("failed to pack `{}`", source.display()))?;
    let lint_warnings = pack_lint_warnings(&source);

    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from(format!("{}.{PACKAGE_EXTENSION}", built.manifest.name)));

    if out.exists() && !args.force {
        bail!(
            "refusing to overwrite `{}` (rerun with --force to replace)",
            out.display()
        );
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create `{}`", parent.display()))?;
    }
    crate::fs_atomic::write_bytes(&out, &built.bytes)
        .with_context(|| format!("failed to write `{}`", out.display()))?;

    let size_bytes = built.bytes.len() as u64;
    let package = SkillPackage {
        manifest: built.manifest,
        hash: built.hash,
        path: out.clone(),
        size_bytes,
    };

    let cached_at = if !args.no_cache {
        let cache = Cache::from_config().context("failed to open cache")?;
        let entry = cache
            .add(&package)
            .context("failed to add package to cache")?;
        Some(entry.package_path)
    } else {
        None
    };

    if ctx.json {
        let payload = PackJson {
            name: &package.manifest.name,
            version: &package.manifest.version,
            path: &package.path,
            files: package.manifest.files.len(),
            size_bytes: package.size_bytes,
            sha256: &package.hash.hex,
            cached_at: cached_at.as_deref(),
            lint_warnings: lint_warnings.len(),
            lint_next_command: (!lint_warnings.is_empty())
                .then(|| format!("agentstack skill lint {}", source.display())),
            skipped_symlinks: &built.skipped_symlinks,
            next_command: unpack_next_command(&package.path),
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    ctx.say(format!("packed {}", package.path.display()));
    ctx.say(format!("  name:        {}", package.manifest.name));
    ctx.say(format!("  version:     {}", package.manifest.version));
    ctx.say(format!("  files:       {}", package.manifest.files.len()));
    ctx.say(format!(
        "  size:        {}",
        crate::cache::human_bytes(package.size_bytes)
    ));
    ctx.say(format!("  sha256:      {}", package.hash.hex));
    if let Some(p) = &cached_at {
        ctx.say(format!("  cached at:   {}", p.display()));
    }
    if !lint_warnings.is_empty() {
        ctx.warn(format!(
            "warning: archive written with {} lint warning{}; run `agentstack skill lint {}`",
            lint_warnings.len(),
            if lint_warnings.len() == 1 { "" } else { "s" },
            source.display()
        ));
    }
    warn_skipped_symlinks(ctx, &built.skipped_symlinks);

    ctx.say("");
    ctx.say("next:");
    ctx.say(format!("  {}", install_next_command(&source)));
    ctx.say(format!("  {}", unpack_next_command(&package.path)));
    if cached_at.is_some() {
        ctx.say("  agentstack cache list");
    }
    Ok(())
}

/// Warn that symlinks in the source tree were excluded from the archive, so the
/// author knows the package is missing content rather than learning it later.
pub(crate) fn warn_skipped_symlinks(ctx: &crate::output::Ctx, skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    ctx.warn(format!(
        "warning: {} symlink{} excluded from the package (packaging copies only regular files): {}",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" },
        skipped.join(", ")
    ));
}

fn pack_lint_warnings(source: &Path) -> Vec<crate::skill::LintWarning> {
    let outcome = validate_skill(source);
    match (outcome.parsed.as_ref(), outcome.content.as_deref()) {
        (Some(parsed), Some(content)) => {
            lint_skill(source, parsed, content, &LintConfig::default())
        }
        _ => Vec::new(),
    }
}

fn unpack_next_command(package_path: &Path) -> String {
    format!(
        "agentstack skill unpack {} --out ./skills",
        package_path.display()
    )
}

fn install_next_command(source: &Path) -> String {
    format!(
        "agentstack skill install {} --target local",
        source.display()
    )
}

#[derive(Serialize)]
struct PackJson<'a> {
    name: &'a str,
    version: &'a str,
    path: &'a Path,
    files: usize,
    size_bytes: u64,
    sha256: &'a str,
    cached_at: Option<&'a Path>,
    lint_warnings: usize,
    lint_next_command: Option<String>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    skipped_symlinks: &'a [String],
    next_command: String,
}
