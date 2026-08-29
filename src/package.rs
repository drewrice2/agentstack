//! Skill package primitives.
//!
//! - [`build_skill_package`] validates a skill directory and produces the
//!   deterministic gzipped tar in memory along with its SHA-256 hash.
//! - [`unpack_package`] extracts and validates an archive on disk under a
//!   parent destination; [`unpack_verified_bytes`] does the same for an
//!   in-memory archive whose hash the caller has already verified.
//! - [`PackageManifest`] / [`SkillPackage`] / [`PackageHash`] are the typed
//!   surfaces used by callers.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::skill::{SKILL_MD, validate_skill, validate_skill_with_expected_dir_name};

/// Default version tag attached to locally-packed skills.
pub const LOCAL_VERSION: &str = "local-dev";

/// Algorithm tag stored on every [`PackageHash`].
pub const HASH_ALGORITHM: &str = "sha256";

/// Conventional file extension for AgentStack skill packages.
pub const PACKAGE_EXTENSION: &str = "tar.gz";

/// Number of hex characters used for short hash references.
pub const SHORT_HASH_LEN: usize = 12;

/// Maximum compressed archive size accepted for pack/unpack.
pub const MAX_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;

/// Maximum number of tar entries accepted in a package.
pub const MAX_ARCHIVE_ENTRIES: usize = 1_000;

/// Maximum bytes extracted from a single archive file entry.
pub const MAX_EXTRACTED_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Maximum total bytes extracted from one archive.
pub const MAX_TOTAL_EXTRACTED_BYTES: u64 = 100 * 1024 * 1024;

/// Cryptographic digest of the bytes on disk for a packaged skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageHash {
    pub algorithm: String,
    pub hex: String,
}

impl PackageHash {
    pub fn sha256_of(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let hex = hex_digest(&digest);
        Self {
            algorithm: HASH_ALGORITHM.to_string(),
            hex,
        }
    }

    /// Short hex prefix used in cache paths and human-readable output.
    pub fn short(&self) -> String {
        self.hex.chars().take(SHORT_HASH_LEN).collect()
    }
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// Static description of a packaged skill — the bits a registry would later
/// also serve back to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Forward-slash relative paths inside the archive, sorted.
    pub files: Vec<String>,
}

/// A skill package that has been written to disk.
#[derive(Debug, Clone)]
pub struct SkillPackage {
    pub manifest: PackageManifest,
    pub hash: PackageHash,
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// Result of [`unpack_package`].
#[derive(Debug, Clone)]
pub struct UnpackedSkill {
    pub manifest: PackageManifest,
    pub hash: PackageHash,
    pub out_path: PathBuf,
}

/// A skill that has been packed in memory but not yet written to disk.
#[derive(Debug, Clone)]
pub struct BuiltPackage {
    pub manifest: PackageManifest,
    pub hash: PackageHash,
    pub bytes: Vec<u8>,
    /// Relative paths of symlinks (and other non-regular entries) found in the
    /// source tree and excluded from the archive. Surfaced so callers can warn
    /// the author that the package is missing content rather than dropping it
    /// silently.
    pub skipped_symlinks: Vec<String>,
}

/// Validate `source` and return the SHA-256 hash for the deterministic
/// gzipped tar archive without materializing the archive bytes.
pub fn hash_skill_package(source: &Path) -> Result<PackageHash> {
    let prepared = prepare_skill_package(source)?;
    let hashing_writer = build_targz_with_writer(
        &prepared.root_name,
        &prepared.entries,
        Sha256Writer::default(),
    )?;
    if hashing_writer.bytes_written > MAX_ARCHIVE_BYTES {
        bail!(
            "package archive is {} bytes; the limit is {MAX_ARCHIVE_BYTES}",
            hashing_writer.bytes_written
        );
    }
    Ok(hashing_writer.into_hash())
}

/// Validate `source` and produce the deterministic gzipped tar archive in
/// memory along with its SHA-256 hash. Callers decide whether to persist
/// the bytes to disk (`agentstack skill pack`) or send them straight to the
/// registry (`agentstack skill push`).
pub fn build_skill_package(source: &Path) -> Result<BuiltPackage> {
    let prepared = prepare_skill_package(source)?;
    let bytes = build_targz(&prepared.root_name, &prepared.entries)?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "package archive is {} bytes; the limit is {MAX_ARCHIVE_BYTES}",
            bytes.len()
        );
    }
    let hash = PackageHash::sha256_of(&bytes);

    Ok(BuiltPackage {
        manifest: prepared.package_manifest,
        hash,
        bytes,
        skipped_symlinks: prepared.skipped_symlinks,
    })
}

fn prepare_skill_package(source: &Path) -> Result<PreparedPackage> {
    let outcome = validate_skill(source);
    if !outcome.is_ok() {
        let first = outcome
            .errors
            .first()
            .map(|e| e.message.as_str())
            .unwrap_or("unknown error");
        bail!("`{}` is not a valid skill: {first}", source.display());
    }
    let manifest = outcome
        .manifest()
        .ok_or_else(|| anyhow!("skill validated but no manifest could be extracted"))?;

    let canonical_source = fs::canonicalize(source)
        .with_context(|| format!("failed to read `{}`", source.display()))?;

    let CollectedEntries {
        mut entries,
        mut skipped_symlinks,
    } = collect_entries(&canonical_source)
        .with_context(|| format!("failed to scan `{}`", source.display()))?;
    entries.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    skipped_symlinks.sort();

    if entries.len() > MAX_ARCHIVE_ENTRIES {
        bail!(
            "package contains {} entries; the limit is {MAX_ARCHIVE_ENTRIES}",
            entries.len()
        );
    }
    if !entries
        .iter()
        .any(|e| e.kind == EntryKind::File && e.archive_path == SKILL_MD)
    {
        bail!("`{}` does not contain SKILL.md", source.display());
    }

    let root_name = manifest.name.clone();
    let package_manifest = PackageManifest {
        name: manifest.name,
        description: manifest.description,
        version: LOCAL_VERSION.to_string(),
        files: entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .map(|e| e.archive_path.clone())
            .collect(),
    };

    Ok(PreparedPackage {
        root_name,
        package_manifest,
        entries,
        skipped_symlinks,
    })
}

/// Extract archive bytes after the caller has already verified their hash.
pub fn unpack_verified_bytes(
    bytes: &[u8],
    out: &Path,
    force: bool,
    hash: PackageHash,
) -> Result<UnpackedSkill> {
    ensure_archive_size(bytes)?;
    extract_targz(bytes, out, force, hash)
}

/// Extract `archive` into parent directory `out` and validate the result.
///
/// Refuses to replace a non-empty final skill directory unless `force` is set.
/// Returns the package manifest plus the SHA-256 of the archive bytes.
pub fn unpack_package(archive: &Path, out: &Path, force: bool) -> Result<UnpackedSkill> {
    let file = fs::File::open(archive)
        .with_context(|| format!("failed to read `{}`", archive.display()))?;
    let bytes = read_archive_with_limit(file, MAX_ARCHIVE_BYTES)?;
    let hash = PackageHash::sha256_of(&bytes);
    extract_targz(&bytes, out, force, hash)
}

fn read_archive_with_limit<R: Read>(reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut reader = reader.take(limit.saturating_add(1) as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .context("failed to read archive")?;
    if bytes.len() > limit {
        bail!("archive exceeded the size limit of {limit} bytes");
    }
    Ok(bytes)
}

fn ensure_archive_size(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_ARCHIVE_BYTES {
        bail!(
            "archive is {} bytes; the limit is {MAX_ARCHIVE_BYTES}",
            bytes.len()
        );
    }
    Ok(())
}

fn extract_targz(
    bytes: &[u8],
    out: &Path,
    force: bool,
    hash: PackageHash,
) -> Result<UnpackedSkill> {
    validate_parent_destination(out)?;
    let staging = create_staging_dir(out)?;

    let result = extract_targz_into(bytes, &staging, hash).and_then(|mut unpacked| {
        let final_out = out.join(&unpacked.manifest.name);
        validate_destination(&final_out, force)?;
        commit_staging(&staging, &final_out)?;
        unpacked.out_path = final_out;
        Ok(unpacked)
    });

    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }

    result
}

fn extract_targz_into(bytes: &[u8], out: &Path, hash: PackageHash) -> Result<UnpackedSkill> {
    let mut tar = tar::Archive::new(GzDecoder::new(bytes));
    let mut root_name: Option<String> = None;
    let mut extracted: Vec<String> = Vec::new();
    let mut entry_count = 0usize;
    let mut total_extracted_bytes = 0u64;

    for entry in tar.entries().context("failed to read tar entries")? {
        let mut entry = entry.context("failed to read tar entry")?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            bail!("archive contains more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        let entry_path = entry
            .path()
            .context("malformed tar entry path")?
            .into_owned();
        ensure_safe_path(&entry_path)?;
        let mut comps = entry_path.components();
        let first = comps
            .next()
            .ok_or_else(|| anyhow!("archive entry has empty path"))?;
        let first_name = first.as_os_str().to_string_lossy().into_owned();
        match &root_name {
            Some(prev) if prev != &first_name => {
                bail!(
                    "archive contains multiple top-level directories: `{prev}` and `{first_name}`"
                );
            }
            None => root_name = Some(first_name.clone()),
            _ => {}
        }
        let entry_type = entry.header().entry_type();
        let rel: PathBuf = comps.collect();
        if rel.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            bail!(
                "archive root entry `{}` must be a directory",
                entry_path.display()
            );
        }
        if entry_type.is_dir() {
            ensure_package_relative_path(&rel, true)?;
        } else if entry_type.is_file() {
            ensure_package_relative_path(&rel, false)?;
        }

        let dest = out.join(&rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create `{}`", parent.display()))?;
        }
        if entry_type.is_dir() {
            fs::create_dir_all(&dest)
                .with_context(|| format!("failed to create `{}`", dest.display()))?;
        } else if entry_type.is_file() {
            let file_size = entry
                .header()
                .size()
                .with_context(|| format!("failed to read size for `{}`", entry_path.display()))?;
            if file_size > MAX_EXTRACTED_FILE_BYTES {
                bail!(
                    "archive entry `{}` is {file_size} bytes; the per-file limit is {MAX_EXTRACTED_FILE_BYTES}",
                    entry_path.display()
                );
            }
            total_extracted_bytes = total_extracted_bytes
                .checked_add(file_size)
                .ok_or_else(|| anyhow!("archive extracted size overflowed"))?;
            if total_extracted_bytes > MAX_TOTAL_EXTRACTED_BYTES {
                bail!(
                    "archive extracts to {total_extracted_bytes} bytes; the limit is {MAX_TOTAL_EXTRACTED_BYTES}"
                );
            }
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .with_context(|| format!("failed to read `{}`", entry_path.display()))?;
            fs::write(&dest, &data)
                .with_context(|| format!("failed to write `{}`", dest.display()))?;
            extracted.push(forward_slashes(&rel));
        } else {
            bail!(
                "archive contains unsupported entry type at `{}`",
                entry_path.display()
            );
        }
    }

    let root_name = root_name.ok_or_else(|| anyhow!("archive is empty"))?;
    extracted.sort();

    let outcome = validate_skill_with_expected_dir_name(out, Some(&root_name));
    if !outcome.is_ok() {
        let first = outcome
            .errors
            .first()
            .map(|e| e.message.as_str())
            .unwrap_or("unknown error");
        bail!("unpacked archive does not validate as a skill: {first}");
    }
    let manifest = outcome
        .manifest()
        .ok_or_else(|| anyhow!("unpacked skill validated but no manifest could be extracted"))?;
    if manifest.name != root_name {
        bail!(
            "archive root `{root_name}` does not match SKILL.md name `{}`",
            manifest.name
        );
    }

    Ok(UnpackedSkill {
        manifest: PackageManifest {
            name: manifest.name,
            description: manifest.description,
            version: LOCAL_VERSION.to_string(),
            files: extracted,
        },
        hash,
        out_path: out.to_path_buf(),
    })
}

fn validate_destination(out: &Path, force: bool) -> Result<()> {
    // Shares the empty-path/symlink/non-directory checks, then layers the
    // replace-safety and non-empty checks that only apply to the final skill
    // directory. `commit_staging` re-checks at replace time (TOCTOU).
    validate_parent_destination(out)?;

    if fs::symlink_metadata(out).is_ok() {
        ensure_replace_target_safe(out)?;
        let has_entries = fs::read_dir(out)
            .with_context(|| format!("failed to read `{}`", out.display()))?
            .next()
            .is_some();
        if has_entries && !force {
            bail!(
                "refusing to overwrite `{}` (rerun with --force to replace)",
                out.display()
            );
        }
    }

    Ok(())
}

fn validate_parent_destination(out: &Path) -> Result<()> {
    if out.as_os_str().is_empty() {
        bail!("destination path must not be empty");
    }

    match fs::symlink_metadata(out) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!(
                    "`{}` is a symlink; refusing to unpack into it",
                    out.display()
                );
            }
            if !metadata.is_dir() {
                bail!("`{}` exists and is not a directory", out.display());
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat `{}`", out.display()));
        }
    }

    Ok(())
}

fn create_staging_dir(out: &Path) -> Result<PathBuf> {
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| if out.has_root() { out } else { Path::new(".") });
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create `{}`", parent.display()))?;

    let raw_name = out.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    crate::fs_atomic::create_unique_dir(parent, ".agentstack-unpack-", raw_name).with_context(
        || {
            format!(
                "failed to create a unique temporary unpack directory under `{}`",
                parent.display()
            )
        },
    )
}

fn commit_staging(staging: &Path, out: &Path) -> Result<()> {
    match fs::symlink_metadata(out) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("`{}` is a symlink; refusing to replace it", out.display());
            }
            if !metadata.is_dir() {
                bail!("`{}` exists and is not a directory", out.display());
            }
            ensure_replace_target_safe(out)?;
            replace_existing_unpack(staging, out)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create `{}`", parent.display()))?;
            }

            fs::rename(staging, out).with_context(|| {
                format!(
                    "failed to move `{}` -> `{}`",
                    staging.display(),
                    out.display()
                )
            })
        }
        Err(err) => Err(err).with_context(|| format!("failed to stat `{}`", out.display())),
    }
}

fn replace_existing_unpack(staging: &Path, out: &Path) -> Result<()> {
    let backup = unique_unpack_backup_path(out)?;
    fs::rename(out, &backup).with_context(|| {
        format!(
            "failed to move existing `{}` -> `{}`",
            out.display(),
            backup.display()
        )
    })?;

    match fs::rename(staging, out) {
        Ok(()) => {
            fs::remove_dir_all(&backup)
                .with_context(|| format!("failed to remove `{}`", backup.display()))?;
            Ok(())
        }
        Err(unpack_err) => {
            let restore = fs::rename(&backup, out);
            if let Err(restore_err) = restore {
                bail!(
                    "failed to move `{}` -> `{}`: {}; also failed to restore `{}` -> `{}`: {}",
                    staging.display(),
                    out.display(),
                    unpack_err,
                    backup.display(),
                    out.display(),
                    restore_err,
                );
            }
            Err(unpack_err).with_context(|| {
                format!(
                    "failed to move `{}` -> `{}`",
                    staging.display(),
                    out.display()
                )
            })
        }
    }
}

fn unique_unpack_backup_path(destination: &Path) -> Result<PathBuf> {
    crate::fs_atomic::reserve_sibling_path(destination, ".agentstack-unpack-backup-").with_context(
        || {
            format!(
                "failed to create a unique temporary backup path next to `{}`",
                destination.display()
            )
        },
    )
}

fn ensure_replace_target_safe(out: &Path) -> Result<()> {
    let canonical =
        fs::canonicalize(out).with_context(|| format!("failed to resolve `{}`", out.display()))?;
    if canonical.parent().is_none() {
        bail!("refusing to replace filesystem root `{}`", out.display());
    }
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    let canonical_cwd = fs::canonicalize(&cwd)
        .with_context(|| format!("failed to resolve current directory `{}`", cwd.display()))?;
    if canonical == canonical_cwd {
        bail!("refusing to replace current directory `{}`", out.display());
    }
    if canonical_cwd.starts_with(&canonical) {
        bail!(
            "refusing to replace ancestor of current directory `{}`",
            out.display()
        );
    }
    Ok(())
}

/// True if a directory with this name should be excluded from packages.
pub fn is_excluded_dir(name: &str) -> bool {
    if is_hidden_name(name) {
        return true;
    }
    matches!(name, "target" | "node_modules" | "__pycache__")
}

/// True if a file with this basename should be excluded from packages.
pub fn is_excluded_file(name: &str) -> bool {
    if is_hidden_name(name) || matches!(name, "Thumbs.db") {
        return true;
    }
    if is_secret_like_file(name) {
        return true;
    }
    false
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

fn is_secret_like_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
        || lower.ends_with(".jks")
        || lower.ends_with(".keystore")
        || lower.ends_with(".ppk")
        || lower.ends_with(".env")
    {
        return true;
    }
    matches!(
        lower.as_str(),
        "tokens.json"
            | "credentials.json"
            | "credentials.yaml"
            | "credentials.yml"
            | "credentials.toml"
            | "credentials.local"
            | "kubeconfig"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
    )
}

struct Entry {
    fs_path: PathBuf,
    archive_path: String,
    kind: EntryKind,
}

struct PreparedPackage {
    root_name: String,
    package_manifest: PackageManifest,
    entries: Vec<Entry>,
    skipped_symlinks: Vec<String>,
}

struct CollectedEntries {
    entries: Vec<Entry>,
    skipped_symlinks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
}

fn collect_entries(root: &Path) -> Result<CollectedEntries> {
    let mut entries = Vec::new();
    let mut skipped_symlinks = Vec::new();
    walk(root, root, &mut entries, &mut skipped_symlinks)?;
    Ok(CollectedEntries {
        entries,
        skipped_symlinks,
    })
}

fn walk(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<Entry>,
    skipped_symlinks: &mut Vec<String>,
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
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if ft.is_dir() {
            if is_excluded_dir(&name_str) {
                continue;
            }
            entries.push(Entry {
                fs_path: path.clone(),
                archive_path: forward_slashes(rel),
                kind: EntryKind::Directory,
            });
            walk(root, &path, entries, skipped_symlinks)?;
        } else if ft.is_file() {
            if is_excluded_file(&name_str) {
                continue;
            }
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to stat `{}`", path.display()))?;
            if metadata.len() > MAX_EXTRACTED_FILE_BYTES {
                bail!(
                    "`{}` is {} bytes; the per-file package limit is {MAX_EXTRACTED_FILE_BYTES}",
                    path.display(),
                    metadata.len()
                );
            }
            entries.push(Entry {
                fs_path: path.clone(),
                archive_path: forward_slashes(rel),
                kind: EntryKind::File,
            });
        } else {
            // Symlinks (and any other non-regular entry) are excluded from the
            // archive — packaging only handles regular files and directories.
            // Record them so callers can warn the author the package is
            // missing content rather than dropping it silently. Excluded
            // names (e.g. node_modules) are not reported as missing.
            if ft.is_symlink() && !is_excluded_file(&name_str) && !is_excluded_dir(&name_str) {
                skipped_symlinks.push(forward_slashes(rel));
            }
        }
    }
    Ok(())
}

fn build_targz(root_name: &str, entries: &[Entry]) -> Result<Vec<u8>> {
    let buf: Vec<u8> = Vec::new();
    build_targz_with_writer(root_name, entries, buf)
}

fn read_regular_file_without_following(path: &Path) -> Result<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let mut file = options
        .open(path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect `{}`", path.display()))?;

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!(
                "`{}` is a reparse point, not a regular file",
                path.display()
            );
        }
    }

    if !metadata.is_file() {
        bail!("`{}` is not a regular file", path.display());
    }

    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    Ok(data)
}

fn build_targz_with_writer<W: Write>(root_name: &str, entries: &[Entry], writer: W) -> Result<W> {
    let gz = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(0xff)
        .write(writer, Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mut total_file_bytes = 0u64;

    for e in entries {
        let data = match e.kind {
            EntryKind::Directory => Vec::new(),
            EntryKind::File => {
                let data = read_regular_file_without_following(&e.fs_path)?;
                if data.len() as u64 > MAX_EXTRACTED_FILE_BYTES {
                    bail!(
                        "`{}` is {} bytes; the per-file package limit is {MAX_EXTRACTED_FILE_BYTES}",
                        e.fs_path.display(),
                        data.len()
                    );
                }
                total_file_bytes = total_file_bytes
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("package contents size overflowed"))?;
                if total_file_bytes > MAX_TOTAL_EXTRACTED_BYTES {
                    bail!(
                        "package contents total {total_file_bytes} bytes; the limit is {MAX_TOTAL_EXTRACTED_BYTES}"
                    );
                }
                data
            }
        };
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(match e.kind {
            EntryKind::Directory => 0o755,
            EntryKind::File => 0o644,
        });
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_entry_type(match e.kind {
            EntryKind::Directory => tar::EntryType::Directory,
            EntryKind::File => tar::EntryType::Regular,
        });
        let archive_path = format!("{root_name}/{}", e.archive_path);
        header
            .set_path(&archive_path)
            .with_context(|| format!("failed to set archive path `{archive_path}`"))?;
        header.set_cksum();
        tar.append(&header, data.as_slice())
            .with_context(|| format!("failed to append `{archive_path}`"))?;
    }
    tar.finish().context("failed to finalize tar archive")?;
    let gz = tar
        .into_inner()
        .context("failed to recover gzip writer from tar builder")?;
    let writer = gz.finish().context("failed to finalize gzip stream")?;
    Ok(writer)
}

#[derive(Default)]
struct Sha256Writer {
    hasher: Sha256,
    bytes_written: usize,
}

impl Sha256Writer {
    fn into_hash(self) -> PackageHash {
        let digest = self.hasher.finalize();
        PackageHash {
            algorithm: HASH_ALGORITHM.to_string(),
            hex: hex_digest(&digest),
        }
    }
}

impl Write for Sha256Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.bytes_written = self
            .bytes_written
            .checked_add(buf.len())
            .ok_or_else(|| std::io::Error::other("package archive size overflowed"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reject paths that could escape an extraction or overlay root: only plain
/// (`Normal`) components are allowed; `..`, absolute roots, and prefixes fail.
pub(crate) fn ensure_safe_path(path: &Path) -> Result<()> {
    for c in path.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => continue,
            _ => bail!("archive contains unsafe path: `{}`", path.display()),
        }
    }
    Ok(())
}

fn ensure_package_relative_path(path: &Path, is_dir: bool) -> Result<()> {
    let components = normal_components(path);
    if components.is_empty() {
        return Ok(());
    }

    let directory_components = if is_dir {
        &components[..]
    } else {
        &components[..components.len() - 1]
    };
    for component in directory_components {
        if is_excluded_dir(component) {
            bail!(
                "archive contains excluded package directory `{}`",
                path.display()
            );
        }
    }

    if !is_dir {
        let file_name = components.last().expect("checked non-empty");
        if is_excluded_file(file_name) {
            bail!(
                "archive contains excluded package file `{}`",
                path.display()
            );
        }
    }

    Ok(())
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn forward_slashes(path: &Path) -> String {
    normal_components(path).join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentstack-package-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn excluded_dirs_match_known_junk() {
        assert!(is_excluded_dir(".git"));
        assert!(is_excluded_dir("target"));
        assert!(is_excluded_dir("node_modules"));
        assert!(!is_excluded_dir("references"));
        assert!(is_excluded_dir(".cache"));
    }

    #[test]
    fn excluded_files_match_known_junk() {
        assert!(is_excluded_file(".DS_Store"));
        assert!(is_excluded_file(".env"));
        assert!(is_excluded_file(".env.local"));
        assert!(is_excluded_file(".npmrc"));
        assert!(is_excluded_file(".netrc"));
        assert!(is_excluded_file(".pypirc"));
        assert!(is_excluded_file("private.pem"));
        assert!(is_excluded_file("server.key"));
        assert!(is_excluded_file("tokens.json"));
        assert!(is_excluded_file("id_rsa"));
        assert!(is_excluded_file("credentials.yml"));
        assert!(is_excluded_file("credentials.local"));
        assert!(!is_excluded_file("README.md"));
        assert!(!is_excluded_file("credentials-guide.md"));
    }

    #[test]
    fn package_hash_short_is_prefix_of_full() {
        let hash = PackageHash::sha256_of(b"hello");
        assert_eq!(hash.algorithm, HASH_ALGORITHM);
        assert_eq!(hash.hex.len(), 64);
        assert!(hash.hex.starts_with(&hash.short()));
        assert_eq!(hash.short().len(), SHORT_HASH_LEN);
    }

    #[test]
    fn read_archive_with_limit_rejects_after_reading_one_extra_byte() {
        let mut reader = std::io::Cursor::new(b"0123456789");

        let err = read_archive_with_limit(&mut reader, 5).unwrap_err();

        assert!(err.to_string().contains("archive exceeded"));
        assert_eq!(reader.position(), 6);
    }

    #[test]
    fn hash_skill_package_matches_built_archive_hash() {
        let dir = test_dir("hash-only");
        let skill = dir.join("hash-only");
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(
            skill.join(SKILL_MD),
            "---\nname: hash-only\ndescription: Use when hashing packages\n---\n",
        )
        .unwrap();
        fs::write(skill.join("references").join("note.md"), "hello").unwrap();

        let built = build_skill_package(&skill).unwrap();
        let hash_only = hash_skill_package(&skill).unwrap();

        assert_eq!(hash_only, built.hash);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn build_targz_rejects_file_replaced_by_symlink() {
        let dir = test_dir("file-replaced-by-symlink");
        let skill = dir.join("skill");
        let file = skill.join("regular.txt");
        let outside_secret = dir.join("outside-secret.txt");
        fs::create_dir_all(&skill).unwrap();
        fs::write(&file, "safe content").unwrap();
        fs::write(&outside_secret, "secret content").unwrap();

        let collected = collect_entries(&skill).unwrap();
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&outside_secret, &file).unwrap();

        let err = build_targz("skill", &collected.entries).unwrap_err();

        assert!(err.to_string().contains("failed to read"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn commit_staging_restores_existing_destination_if_final_rename_fails() {
        let dir = test_dir("rollback");
        let out = dir.join("round-trip");
        let staging = dir.join("missing-staging");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("keep.txt"), "keep").unwrap();

        let err = commit_staging(&staging, &out).unwrap_err();

        assert!(err.to_string().contains("failed to move"));
        assert_eq!(fs::read_to_string(out.join("keep.txt")).unwrap(), "keep");
        let backup_count = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".agentstack-unpack-backup-")
            })
            .count();
        assert_eq!(backup_count, 0);
        let _ = fs::remove_dir_all(dir);
    }
}
