//! Local package cache.
//!
//! Layout (under `~/.agentstack/cache` / `AGENTSTACK_CACHE_DIR`):
//!
//! ```text
//! skills/
//!   <skill-name>/
//!     <short-hash>/
//!       package.tar.gz
//!       manifest.json
//! ```
//!
//! Each `manifest.json` is the serialized [`CacheEntryFile`] (the on-disk
//! shape of a [`CacheEntry`] without the path, which is implied by location).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::package::{PackageHash, PackageManifest, SkillPackage};

/// Subdirectory under the cache root that holds skill packages.
pub const SKILLS_DIR: &str = "skills";
/// Filename used for the package archive inside each cache entry.
pub const PACKAGE_FILE: &str = "package.tar.gz";
/// Filename used for the entry's metadata sidecar.
pub const MANIFEST_FILE: &str = "manifest.json";

/// A package known to the local cache. Constructed by walking the cache
/// directory or returned by [`Cache::add`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheEntry {
    pub manifest: PackageManifest,
    pub hash: PackageHash,
    pub size_bytes: u64,
    pub package_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CacheEntryFile {
    manifest: PackageManifest,
    hash: PackageHash,
    size_bytes: u64,
}

/// Handle to the local skill cache.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Resolve the cache from the default config (or `AGENTSTACK_CACHE_DIR`).
    pub fn from_config() -> Result<Self> {
        Ok(Self::at(config::cache_dir()?))
    }

    /// Construct a cache rooted at an explicit path. Mostly useful for tests.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// Root of the cache, e.g. `~/.agentstack/cache`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding the per-skill subdirectories.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join(SKILLS_DIR)
    }

    /// Path that would store a given package, by name + short hash.
    pub fn entry_dir(&self, name: &str, short_hash: &str) -> PathBuf {
        self.skills_dir().join(name).join(short_hash)
    }

    /// Copy a freshly-packed [`SkillPackage`] into the cache.
    ///
    /// The cache layout is keyed by skill name and the package's short hash,
    /// so re-adding the same content is idempotent.
    pub fn add(&self, package: &SkillPackage) -> Result<CacheEntry> {
        let archive = fs::read(&package.path)
            .with_context(|| format!("failed to read `{}`", package.path.display()))?;
        self.add_archive(package.manifest.clone(), package.hash.clone(), &archive)
    }

    /// Add an already-verified archive to the cache. Used by remote install,
    /// where the registry client has the package bytes in memory.
    pub fn add_archive(
        &self,
        manifest: PackageManifest,
        hash: PackageHash,
        archive: &[u8],
    ) -> Result<CacheEntry> {
        check_name_arg(&manifest.name)?;
        let entry_dir = self.entry_dir(&manifest.name, &hash.short());
        fs::create_dir_all(&entry_dir)
            .with_context(|| format!("failed to create `{}`", entry_dir.display()))?;

        let package_path = entry_dir.join(PACKAGE_FILE);
        crate::fs_atomic::write_bytes(&package_path, archive)
            .with_context(|| format!("failed to write `{}`", package_path.display()))?;

        let entry_file = CacheEntryFile {
            manifest,
            hash,
            size_bytes: archive.len() as u64,
        };
        let manifest_path = entry_dir.join(MANIFEST_FILE);
        let json = serde_json::to_string_pretty(&entry_file)
            .context("failed to serialize cache manifest")?;
        crate::fs_atomic::write_bytes(&manifest_path, json.as_bytes())
            .with_context(|| format!("failed to write `{}`", manifest_path.display()))?;

        Ok(CacheEntry {
            manifest: entry_file.manifest,
            hash: entry_file.hash,
            size_bytes: entry_file.size_bytes,
            package_path,
        })
    }

    /// Enumerate every entry in the cache, sorted by name then short hash.
    pub fn list(&self) -> Result<Vec<CacheEntry>> {
        let skills_dir = self.skills_dir();
        if !skills_dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let entries = fs::read_dir(&skills_dir)
            .with_context(|| format!("failed to read `{}`", skills_dir.display()))?;
        for skill in entries {
            let skill = skill.context("failed to read cache skill entry")?;
            let skill_path = skill.path();
            if !skill_path.is_dir() {
                continue;
            }
            let versions = fs::read_dir(&skill_path)
                .with_context(|| format!("failed to read `{}`", skill_path.display()))?;
            for version in versions {
                let version = version.context("failed to read cache version entry")?;
                let version_path = version.path();
                if !version_path.is_dir() {
                    continue;
                }
                let manifest_path = version_path.join(MANIFEST_FILE);
                let package_path = version_path.join(PACKAGE_FILE);
                if !manifest_path.is_file() || !package_path.is_file() {
                    continue;
                }
                let json = fs::read_to_string(&manifest_path)
                    .with_context(|| format!("failed to read `{}`", manifest_path.display()))?;
                let file: CacheEntryFile = serde_json::from_str(&json)
                    .with_context(|| format!("failed to parse `{}`", manifest_path.display()))?;
                out.push(CacheEntry {
                    manifest: file.manifest,
                    hash: file.hash,
                    size_bytes: file.size_bytes,
                    package_path,
                });
            }
        }
        out.sort_by(|a, b| {
            a.manifest
                .name
                .cmp(&b.manifest.name)
                .then_with(|| a.hash.hex.cmp(&b.hash.hex))
        });
        Ok(out)
    }

    /// Remove every cached version of `name`. Returns true if anything was
    /// removed; false if nothing matched.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let dir = self.skills_dir().join(name);
        if !dir.exists() {
            return Ok(false);
        }
        if !dir.is_dir() {
            bail!("`{}` exists but is not a directory", dir.display());
        }
        fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to remove `{}`", dir.display()))?;
        Ok(true)
    }
}

/// Convenience helper used by display code: human-readable size string.
pub fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    if n == 0 {
        return "0 B".into();
    }
    let mut size = n as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Hint shown by the CLI when the cache root exists but contains no skills.
pub fn empty_message(cache: &Cache) -> String {
    format!("(cache is empty at {})", cache.root().display())
}

/// Surface a typed error when a name is empty or contains a path separator.
pub fn check_name_arg(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("skill name must not be empty"));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(anyhow!(
            "skill name `{name}` contains path separators or `..`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_examples() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn check_name_rejects_separators() {
        assert!(check_name_arg("ok-name").is_ok());
        assert!(check_name_arg("").is_err());
        assert!(check_name_arg("a/b").is_err());
        assert!(check_name_arg("..").is_err());
        assert!(check_name_arg("foo/../bar").is_err());
    }
}
