//! Install targets — the named local destinations AgentStack can copy a skill
//! into.
//!
//! AgentStack is intentionally cautious about hard-coding platform paths: we
//! ship best-effort defaults derived from `$HOME`, but the source of truth is
//! always the user's `config.toml`. [`TargetResolver::resolve`] makes the
//! provenance explicit so callers can tell users *why* a path was picked.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::BaseDirs;
use serde::Serialize;

use crate::config::ConfigStore;
use crate::error::CliError;

/// Named install target. Stable string forms are used as TOML keys and CLI
/// arguments, so the spelling here is part of the public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstallTarget {
    ClaudeCode,
    Codex,
    RepoClaudeCode,
    RepoCodex,
    Local,
}

impl InstallTarget {
    /// Every supported target, in display order.
    pub const ALL: &'static [InstallTarget] = &[
        InstallTarget::ClaudeCode,
        InstallTarget::Codex,
        InstallTarget::RepoClaudeCode,
        InstallTarget::RepoCodex,
        InstallTarget::Local,
    ];

    /// Stable lowercase identifier used on the CLI and in `config.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            InstallTarget::ClaudeCode => "claude-code",
            InstallTarget::Codex => "codex",
            InstallTarget::RepoClaudeCode => "repo-claude-code",
            InstallTarget::RepoCodex => "repo-codex",
            InstallTarget::Local => "local",
        }
    }

    /// Friendlier phrase-order spelling accepted by install/update commands.
    pub const fn alias(self) -> Option<&'static str> {
        match self {
            InstallTarget::RepoClaudeCode => Some("claude-code-repo"),
            InstallTarget::RepoCodex => Some("codex-repo"),
            _ => None,
        }
    }

    /// Platform name used for `platform/<name>/` overlays and platform tag
    /// matching. Derived from the canonical target name: repo-scoped targets
    /// share their user-level platform, and `local` is platform-agnostic.
    pub fn platform(self) -> Option<&'static str> {
        match self {
            InstallTarget::Local => None,
            other => {
                let name = other.as_str();
                Some(name.strip_prefix("repo-").unwrap_or(name))
            }
        }
    }

    /// One-line description used by `agentstack target list`.
    pub const fn description(self) -> &'static str {
        match self {
            InstallTarget::ClaudeCode => "Anthropic Claude Code user skills",
            InstallTarget::Codex => "OpenAI Codex CLI skills",
            InstallTarget::RepoClaudeCode => "Claude Code repo skills in the current repo",
            InstallTarget::RepoCodex => "Codex repo skills in the current repo",
            InstallTarget::Local => "AgentStack local skill library (target-agnostic)",
        }
    }

    /// Parse canonical target names plus documented repo target aliases.
    pub fn parse(name: &str) -> Result<Self> {
        let normalized = name.trim().to_ascii_lowercase();
        let normalized = normalized.replace('_', "-");
        match normalized.as_str() {
            "claude-code" => Ok(InstallTarget::ClaudeCode),
            "codex" => Ok(InstallTarget::Codex),
            "repo-claude-code" | "claude-code-repo" => Ok(InstallTarget::RepoClaudeCode),
            "repo-codex" | "codex-repo" => Ok(InstallTarget::RepoCodex),
            "local" => Ok(InstallTarget::Local),
            _ => Err(CliError::new(
                "invalid_target",
                format!(
                    "unknown install target `{name}` (expected one of: {}; aliases: codex-repo, claude-code-repo)",
                    Self::ALL
                        .iter()
                        .map(|t| t.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .resource(name)
            .action("configure_target")
            .next_command("agentstack target list")
            .into()),
        }
    }
}

impl std::fmt::Display for InstallTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a [`ResolvedTarget`]'s path came from. Surfaced in CLI output so
/// users can tell a default apart from a configured override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSource {
    /// Path came from `config.toml`.
    Override,
    /// Path came from the platform default (best-effort, `$HOME`-derived).
    Default,
}

impl TargetSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            TargetSource::Override => "override",
            TargetSource::Default => "default",
        }
    }
}

/// A resolved install target — the path AgentStack will write into.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub target: InstallTarget,
    pub path: PathBuf,
    pub source: TargetSource,
}

/// Filesystem readiness for one install target.
#[derive(Debug, Clone, Serialize)]
pub struct TargetDetection {
    pub target: &'static str,
    pub description: &'static str,
    pub configured: bool,
    pub path: Option<PathBuf>,
    pub source: &'static str,
    pub absolute: bool,
    pub exists: bool,
    pub is_dir: bool,
    pub writable: bool,
    pub creatable: bool,
    pub usable: bool,
    pub fix_command: Option<String>,
}

/// Resolves [`InstallTarget`] values into concrete paths, consulting the
/// [`ConfigStore`] first and falling back to platform defaults.
pub struct TargetResolver<'a> {
    store: &'a ConfigStore,
}

impl<'a> TargetResolver<'a> {
    pub fn new(store: &'a ConfigStore) -> Self {
        Self { store }
    }

    /// Resolve `target` to a concrete path, preferring config overrides.
    /// Returns an error only if neither an override nor a default is available.
    pub fn resolve(&self, target: InstallTarget) -> Result<ResolvedTarget> {
        if let Some(p) = self.store.target_override(target.as_str()) {
            if !p.is_absolute() {
                return Err(anyhow!(
                    "target override path for `{}` must be absolute (got `{}`); fix with `agentstack target set {} --path <absolute-path>`",
                    target.as_str(),
                    p.display(),
                    target.as_str(),
                ));
            }
            return Ok(ResolvedTarget {
                target,
                path: p.to_path_buf(),
                source: TargetSource::Override,
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
        Ok(ResolvedTarget {
            target,
            path,
            source: TargetSource::Default,
        })
    }

    /// Resolve every known target. Each entry is `Ok` if the target has a
    /// path (override or default) and `Err` if neither is available.
    pub fn resolve_all(&self) -> Vec<(InstallTarget, Result<ResolvedTarget>)> {
        InstallTarget::ALL
            .iter()
            .map(|t| (*t, self.resolve(*t)))
            .collect()
    }

    /// Inspect every known target without mutating configuration.
    pub fn detect_all(&self) -> Vec<TargetDetection> {
        InstallTarget::ALL
            .iter()
            .map(|target| self.detect(*target))
            .collect()
    }

    /// Inspect one target without falling relative overrides back to defaults.
    pub fn detect(&self, target: InstallTarget) -> TargetDetection {
        let override_path = self.store.target_override(target.as_str());
        let (configured, path, source) = match override_path {
            Some(path) => (
                true,
                Some(path.to_path_buf()),
                TargetSource::Override.as_str(),
            ),
            None => match default_target_path(target) {
                Some(path) => (false, Some(path), TargetSource::Default.as_str()),
                None => (false, None, "missing"),
            },
        };

        let mut absolute = false;
        let mut exists = false;
        let mut is_dir = false;
        let mut writable = false;
        let mut creatable = false;

        if let Some(path) = &path {
            absolute = path.is_absolute();
            if absolute {
                exists = path.exists();
                if exists {
                    is_dir = path.is_dir();
                    writable = is_dir && writable_dir(path);
                } else {
                    creatable = parent_creatable(path);
                }
            }
        }

        let usable = absolute && (writable || creatable);
        let fix_command = fix_command(target, path.as_deref(), configured, usable);

        TargetDetection {
            target: target.as_str(),
            description: target.description(),
            configured,
            path,
            source,
            absolute,
            exists,
            is_dir,
            writable,
            creatable,
            usable,
            fix_command,
        }
    }
}

/// Compute the platform default for `target`. Pulled out so it can be unit
/// tested without a [`ConfigStore`].
pub fn default_target_path(target: InstallTarget) -> Option<PathBuf> {
    let path = match target {
        InstallTarget::RepoClaudeCode => repo_root_or_current_dir()?.join(".claude").join("skills"),
        InstallTarget::RepoCodex => repo_root_or_current_dir()?.join(".codex").join("skills"),
        InstallTarget::ClaudeCode => BaseDirs::new()?.home_dir().join(".claude").join("skills"),
        InstallTarget::Codex => BaseDirs::new()?.home_dir().join(".codex").join("skills"),
        InstallTarget::Local => BaseDirs::new()?
            .home_dir()
            .join(".agentstack")
            .join("skills"),
    };
    Some(path)
}

fn repo_root_or_current_dir() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return std::env::current_dir().ok();
        }
    }
}

fn fix_command(
    target: InstallTarget,
    path: Option<&Path>,
    configured: bool,
    usable: bool,
) -> Option<String> {
    if configured && usable {
        return None;
    }
    let path = path
        .filter(|p| p.is_absolute())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<absolute-path>".to_string());
    if !configured && usable {
        return Some(format!(
            "agentstack target setup {} --path {} --yes",
            target.as_str(),
            path
        ));
    }
    // A configured-but-unusable target already points at a bad path; echoing it
    // back would just re-apply the broken value, so ask for a fresh path.
    let suggest_path = if configured {
        "<absolute-path>".to_string()
    } else {
        path
    };
    Some(format!(
        "agentstack target set {} --path {}",
        target.as_str(),
        suggest_path
    ))
}

pub(crate) fn parent_creatable(path: &Path) -> bool {
    nearest_existing_dir(path).is_some_and(writable_dir)
}

fn nearest_existing_dir(path: &Path) -> Option<&Path> {
    let mut current = path.parent();
    while let Some(dir) = current {
        match fs::metadata(dir) {
            Ok(metadata) if metadata.is_dir() => return Some(dir),
            Ok(_) => return None,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => current = dir.parent(),
            Err(_) => return None,
        }
    }
    None
}

/// Best-effort writability probe — try to create and then drop a temp file.
pub(crate) fn writable_dir(dir: &Path) -> bool {
    let probe = dir.join(format!(".agentstack-target-probe-{}", std::process::id()));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(mut file) => {
            let _ = file.write_all(b"");
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_canonical_and_documented_aliases() {
        assert_eq!(
            InstallTarget::parse("claude-code").unwrap(),
            InstallTarget::ClaudeCode
        );
        assert_eq!(
            InstallTarget::parse("CLAUDE_CODE").unwrap(),
            InstallTarget::ClaudeCode
        );
        assert_eq!(InstallTarget::parse("codex").unwrap(), InstallTarget::Codex);
        assert_eq!(
            InstallTarget::parse("claude-code-repo").unwrap(),
            InstallTarget::RepoClaudeCode
        );
        assert_eq!(
            InstallTarget::parse("repo_claude_code").unwrap(),
            InstallTarget::RepoClaudeCode
        );
        assert_eq!(
            InstallTarget::parse("codex-repo").unwrap(),
            InstallTarget::RepoCodex
        );
        assert_eq!(InstallTarget::parse("local").unwrap(), InstallTarget::Local);
    }

    #[test]
    fn platform_maps_targets_to_overlay_names() {
        assert_eq!(InstallTarget::ClaudeCode.platform(), Some("claude-code"));
        assert_eq!(
            InstallTarget::RepoClaudeCode.platform(),
            Some("claude-code")
        );
        assert_eq!(InstallTarget::Codex.platform(), Some("codex"));
        assert_eq!(InstallTarget::RepoCodex.platform(), Some("codex"));
        assert_eq!(InstallTarget::Local.platform(), None);
    }

    #[test]
    fn parse_rejects_unknown_and_undocumented_aliases() {
        for name in [
            "anthropic-cli",
            "claude",
            "repo-claude",
            "claude-repo",
            "project-claude-code",
            "project-codex",
        ] {
            let err = InstallTarget::parse(name).unwrap_err();
            assert!(err.to_string().contains("unknown install target"));
        }
    }

    #[test]
    fn fix_command_does_not_echo_a_broken_configured_path() {
        // A configured-but-unusable target must not suggest re-applying its own
        // broken path; it asks for a fresh one.
        let fix = fix_command(
            InstallTarget::ClaudeCode,
            Some(Path::new("/broken/configured/path")),
            true,
            false,
        )
        .expect("a broken configured target should suggest a fix");
        assert!(
            fix.contains("--path <absolute-path>"),
            "should suggest a placeholder path, got: {fix}"
        );
        assert!(!fix.contains("/broken/configured/path"), "got: {fix}");
    }

    #[test]
    fn override_wins_over_default() {
        let path = std::env::temp_dir().join(format!(
            "astack-tr-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let mut store = ConfigStore::load_from(path).unwrap();
        store.set_target("claude-code".into(), PathBuf::from("/custom/c"));

        let resolver = TargetResolver::new(&store);
        let resolved = resolver.resolve(InstallTarget::ClaudeCode).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/custom/c"));
        assert_eq!(resolved.source, TargetSource::Override);
    }

    #[test]
    fn default_used_when_no_override() {
        let path = std::env::temp_dir().join(format!(
            "astack-tr-d-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let store = ConfigStore::load_from(path).unwrap();
        let resolver = TargetResolver::new(&store);
        // We can only assert structural shape here; the actual path depends
        // on the host's $HOME.
        if let Ok(resolved) = resolver.resolve(InstallTarget::Local) {
            assert_eq!(resolved.target, InstallTarget::Local);
            assert_eq!(resolved.source, TargetSource::Default);
            assert!(resolved.path.ends_with("skills"));
        }
    }
}
