use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically write UTF-8 content to `path` by renaming a sibling temp file.
pub(crate) fn write_string(path: &Path, content: &str) -> Result<()> {
    write_string_inner(path, content, None)
}

/// Atomically write UTF-8 content to `path` and set a Unix mode before rename.
pub(crate) fn write_string_with_mode(path: &Path, content: &str, mode_unix: u32) -> Result<()> {
    write_string_inner(path, content, Some(mode_unix))
}

/// Atomically write bytes to `path` by renaming a sibling temp file.
pub(crate) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    write_bytes_inner(path, bytes, None)
}

fn write_string_inner(path: &Path, content: &str, mode_unix: Option<u32>) -> Result<()> {
    write_bytes_inner(path, content.as_bytes(), mode_unix)
}

fn write_bytes_inner(path: &Path, bytes: &[u8], mode_unix: Option<u32>) -> Result<()> {
    let (tmp, mut temp_file) = create_temp_file(path, ".agentstack-", mode_unix)?;
    let result = (|| -> Result<()> {
        temp_file
            .write_all(bytes)
            .with_context(|| format!("failed to write `{}`", tmp.display()))?;
        #[cfg(unix)]
        if let Some(mode) = mode_unix {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&tmp)?.permissions();
            perms.set_mode(mode);
            fs::set_permissions(&tmp, perms)?;
        }
        #[cfg(not(unix))]
        let _ = mode_unix;
        drop(temp_file);
        fs::rename(&tmp, path).with_context(|| {
            format!("failed to move `{}` -> `{}`", tmp.display(), path.display())
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }

    result
}

/// The parent directory to place generated siblings in, treating an empty
/// parent as the current directory.
fn sibling_parent(dest: &Path) -> &Path {
    dest.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// Yield up to 100 candidate paths under `parent` named
/// `{prefix}{sanitized name}-{pid}-{nanos}-{attempt}`.
fn unique_path_candidates<'a>(
    parent: &'a Path,
    prefix: &str,
    name: &str,
) -> impl Iterator<Item = PathBuf> + 'a {
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let pid = std::process::id();
    let prefix = prefix.to_string();
    (0..100).map(move |attempt| parent.join(format!("{prefix}{safe_name}-{pid}-{nanos}-{attempt}")))
}

fn candidates_exhausted() -> io::Error {
    io::Error::new(ErrorKind::AlreadyExists, "exhausted unique path candidates")
}

/// Create a uniquely named directory under `parent`, retrying on name
/// collisions.
pub(crate) fn create_unique_dir(parent: &Path, prefix: &str, name: &str) -> io::Result<PathBuf> {
    for candidate in unique_path_candidates(parent, prefix, name) {
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(candidates_exhausted())
}

/// Pick an unused sibling path for `dest` without creating it.
///
/// Only suitable for rename targets, which cannot be pre-created; the
/// existence check is inherently racy, but the pid + nanos + counter naming
/// makes collisions effectively impossible.
pub(crate) fn reserve_sibling_path(dest: &Path, prefix: &str) -> io::Result<PathBuf> {
    let parent = sibling_parent(dest);
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("skill");
    for candidate in unique_path_candidates(parent, prefix, name) {
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(candidates_exhausted())
}

/// Create a uniquely named sibling file of `dest`, retrying on name
/// collisions. The optional Unix mode is applied at create time so the file
/// is never readable more broadly than intended.
pub(crate) fn create_temp_file(
    dest: &Path,
    prefix: &str,
    mode_unix: Option<u32>,
) -> Result<(PathBuf, fs::File)> {
    let parent = sibling_parent(dest);
    let raw_name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    #[cfg(not(unix))]
    let _ = mode_unix;

    for candidate in unique_path_candidates(parent, prefix, raw_name) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode_unix {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }

        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to create `{}`", candidate.display()));
            }
        }
    }

    bail!(
        "failed to create a unique temporary file next to `{}`",
        dest.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agentstack-fs-atomic-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_string_writes_file_content() {
        let dir = test_dir("write");
        let path = dir.join("config.toml");

        write_string(&path, "registry = {}\n").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "registry = {}\n");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_string_overwrites_existing_file() {
        let dir = test_dir("overwrite");
        let path = dir.join("config.toml");
        fs::write(&path, "old").unwrap();

        write_string(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_bytes_overwrites_existing_file() {
        let dir = test_dir("bytes");
        let path = dir.join("package.tar.gz");
        fs::write(&path, b"old").unwrap();

        write_bytes(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_string_does_not_leave_tmp_file_after_success() {
        let dir = test_dir("cleanup");
        let path = dir.join("config.toml");

        write_string(&path, "content").unwrap();

        let tmp_files = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".agentstack-"))
            .count();
        assert_eq!(tmp_files, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_string_with_mode_sets_destination_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("mode");
        let path = dir.join("tokens.json");

        write_string_with_mode(&path, "{}", 0o600).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn temp_file_for_mode_write_is_restricted_before_content_write() {
        use std::os::unix::fs::PermissionsExt;

        let dir = test_dir("tmp-mode");
        let path = dir.join("tokens.json");
        let (tmp, file) = create_temp_file(&path, ".agentstack-", Some(0o600)).unwrap();

        let mode = fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        drop(file);
        let _ = fs::remove_file(tmp);
        let _ = fs::remove_dir_all(dir);
    }
}
