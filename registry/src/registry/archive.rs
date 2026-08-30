use std::{
    io::Read,
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
};

use flate2::read::GzDecoder;
use serde::Deserialize;

use crate::{error::ServerError, registry::validate_slug};

const MAX_ARCHIVE_ENTRIES: usize = 1_000;
const MAX_EXTRACTED_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_TOTAL_EXTRACTED_BYTES: u64 = 100 * 1024 * 1024;
const MAX_DESCRIPTION_LEN: usize = 500;
const SKILL_MD: &str = "SKILL.md";

#[derive(Debug)]
struct ArchiveMetadata {
    name: String,
    description: String,
}

struct ArchiveSkillMd {
    root_name: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Default, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

pub(crate) async fn validate_archive_metadata_blocking(
    archive: Arc<[u8]>,
    expected_name: String,
    expected_description: String,
) -> Result<(), ServerError> {
    tokio::task::spawn_blocking(move || {
        validate_archive_metadata(archive.as_ref(), &expected_name, &expected_description)
    })
    .await
    .map_err(|err| {
        tracing::error!(error = %err, "archive validation task failed");
        ServerError::internal_error()
    })?
}

fn validate_archive_metadata(
    archive: &[u8],
    expected_name: &str,
    expected_description: &str,
) -> Result<(), ServerError> {
    let archive_metadata = read_archive_metadata(archive)?;
    if archive_metadata.name != expected_name {
        return Err(ServerError::validation_error(format!(
            "metadata.name `{expected_name}` does not match archive SKILL.md name `{}`",
            archive_metadata.name
        )));
    }
    if archive_metadata.description != expected_description {
        return Err(ServerError::validation_error(
            "metadata.description does not match archive SKILL.md description",
        ));
    }
    Ok(())
}

fn read_archive_metadata(archive: &[u8]) -> Result<ArchiveMetadata, ServerError> {
    let skill_md = read_skill_md_entry(archive)?;
    let manifest = parse_skill_md_manifest(&skill_md.bytes)?;
    if manifest.name != skill_md.root_name {
        return Err(ServerError::validation_error(format!(
            "archive root `{}` does not match SKILL.md name `{}`",
            skill_md.root_name, manifest.name
        )));
    }
    Ok(manifest)
}

fn read_skill_md_entry(archive: &[u8]) -> Result<ArchiveSkillMd, ServerError> {
    // Bound the decompressed byte budget independently of tar header claims so
    // a gzip bomb cannot trick the validator into doing unbounded work before
    // the per-entry/total checks below fire.
    let limited = GzDecoder::new(archive).take(MAX_TOTAL_EXTRACTED_BYTES + 1);
    let mut tar = tar::Archive::new(limited);
    let entries = tar
        .entries()
        .map_err(|_| ServerError::validation_error("archive is not a valid tar.gz package"))?;
    let mut root_name: Option<String> = None;
    let mut skill_md: Option<Vec<u8>> = None;
    let mut entry_count = 0usize;
    let mut total_extracted_bytes = 0u64;

    for entry in entries {
        let mut entry =
            entry.map_err(|_| ServerError::validation_error("failed to read archive entry"))?;
        entry_count += 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            return Err(ServerError::validation_error(format!(
                "archive contains more than {MAX_ARCHIVE_ENTRIES} entries"
            )));
        }

        let entry_path = entry
            .path()
            .map_err(|_| ServerError::validation_error("malformed archive entry path"))?
            .into_owned();
        ensure_safe_archive_path(&entry_path)?;

        let mut components = entry_path.components();
        let first = components
            .next()
            .ok_or_else(|| ServerError::validation_error("archive entry has empty path"))?;
        let first_name = first.as_os_str().to_string_lossy().into_owned();
        match &root_name {
            Some(previous) if previous != &first_name => {
                return Err(ServerError::validation_error(format!(
                    "archive contains multiple top-level directories: `{previous}` and `{first_name}`"
                )));
            }
            None => root_name = Some(first_name.clone()),
            _ => {}
        }

        let entry_type = entry.header().entry_type();
        let rel: PathBuf = components.collect();
        if rel.as_os_str().is_empty() {
            if entry_type.is_dir() {
                continue;
            }
            return Err(ServerError::validation_error(format!(
                "archive root entry `{}` must be a directory",
                entry_path.display()
            )));
        }
        if entry_type.is_dir() {
            ensure_package_relative_path(&rel, true)?;
            continue;
        }
        if !entry_type.is_file() {
            return Err(ServerError::validation_error(format!(
                "archive contains unsupported entry type at `{}`",
                entry_path.display()
            )));
        }
        ensure_package_relative_path(&rel, false)?;

        let file_size = entry.header().size().map_err(|_| {
            ServerError::validation_error(format!(
                "failed to read size for archive entry `{}`",
                entry_path.display()
            ))
        })?;
        if file_size > MAX_EXTRACTED_FILE_BYTES {
            return Err(ServerError::validation_error(format!(
                "archive entry `{}` is {file_size} bytes; the per-file limit is {MAX_EXTRACTED_FILE_BYTES}",
                entry_path.display()
            )));
        }
        total_extracted_bytes = total_extracted_bytes
            .checked_add(file_size)
            .ok_or_else(|| ServerError::validation_error("archive extracted size overflowed"))?;
        if total_extracted_bytes > MAX_TOTAL_EXTRACTED_BYTES {
            return Err(ServerError::validation_error(format!(
                "archive extracts to {total_extracted_bytes} bytes; the limit is {MAX_TOTAL_EXTRACTED_BYTES}"
            )));
        }

        if rel == FsPath::new(SKILL_MD) {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(|_| {
                ServerError::validation_error(format!(
                    "failed to read archive entry `{}`",
                    entry_path.display()
                ))
            })?;
            skill_md = Some(data);
        }
    }

    let root_name = root_name.ok_or_else(|| ServerError::validation_error("archive is empty"))?;
    let skill_md =
        skill_md.ok_or_else(|| ServerError::validation_error("archive is missing SKILL.md"))?;
    Ok(ArchiveSkillMd {
        root_name,
        bytes: skill_md,
    })
}

fn ensure_safe_archive_path(path: &FsPath) -> Result<(), ServerError> {
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => continue,
            _ => {
                return Err(ServerError::validation_error(format!(
                    "archive contains unsafe path `{}`",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn ensure_package_relative_path(path: &FsPath, is_dir: bool) -> Result<(), ServerError> {
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
            return Err(ServerError::validation_error(format!(
                "archive contains excluded package directory `{}`",
                path.display()
            )));
        }
    }

    if !is_dir {
        let file_name = components.last().expect("checked non-empty");
        if is_excluded_file(file_name) {
            return Err(ServerError::validation_error(format!(
                "archive contains excluded package file `{}`",
                path.display()
            )));
        }
    }

    Ok(())
}

fn normal_components(path: &FsPath) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn is_excluded_dir(name: &str) -> bool {
    is_hidden_name(name) || matches!(name, "target" | "node_modules" | "__pycache__")
}

fn is_excluded_file(name: &str) -> bool {
    is_hidden_name(name)
        || matches!(name, "Thumbs.db")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || matches!(
            name,
            "tokens.json"
                | "credentials.json"
                | "credentials.yaml"
                | "credentials.yml"
                | "credentials.toml"
                | "credentials.local"
                | "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
        )
}

fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

fn parse_skill_md_manifest(bytes: &[u8]) -> Result<ArchiveMetadata, ServerError> {
    let content = std::str::from_utf8(bytes)
        .map_err(|_| ServerError::validation_error("SKILL.md is not valid UTF-8"))?;
    let yaml = split_frontmatter(content)?;
    let raw: SkillFrontmatter = if yaml.trim().is_empty() {
        SkillFrontmatter::default()
    } else {
        serde_yaml::from_str(yaml)
            .map_err(|_| ServerError::validation_error("SKILL.md frontmatter is malformed YAML"))?
    };

    let name = raw
        .name
        .ok_or_else(|| ServerError::validation_error("SKILL.md name is missing"))?;
    let name = name.trim().to_string();
    validate_slug(&name).map_err(ServerError::validation_error)?;

    let description = raw
        .description
        .ok_or_else(|| ServerError::validation_error("SKILL.md description is missing"))?;
    if description.trim().is_empty() {
        return Err(ServerError::validation_error(
            "SKILL.md description must not be empty",
        ));
    }
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(ServerError::validation_error(format!(
            "SKILL.md description must be at most {MAX_DESCRIPTION_LEN} characters"
        )));
    }

    Ok(ArchiveMetadata { name, description })
}

fn split_frontmatter(content: &str) -> Result<&str, ServerError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let first_nl = content
        .find('\n')
        .ok_or_else(|| ServerError::validation_error("SKILL.md is missing YAML frontmatter"))?;
    if content[..first_nl].trim_end_matches('\r') != "---" {
        return Err(ServerError::validation_error(
            "SKILL.md is missing YAML frontmatter",
        ));
    }

    let yaml_start = first_nl + 1;
    let mut cursor = yaml_start;
    while cursor <= content.len() {
        let line_end = content[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(content.len());
        let line = content[cursor..line_end].trim_end_matches('\r');
        if line == "---" {
            return Ok(content[yaml_start..cursor].trim_end_matches(['\n', '\r']));
        }
        if line_end == content.len() {
            break;
        }
        cursor = line_end + 1;
    }

    Err(ServerError::validation_error(
        "SKILL.md is missing closing YAML frontmatter delimiter",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    use tar::{Builder, Header};

    fn put_file(tar: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, data: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(&mut header, path, data).unwrap();
    }

    fn finish(tar: Builder<GzEncoder<Vec<u8>>>) -> Vec<u8> {
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn archive_exceeding_decompression_ceiling_is_rejected() {
        let gz = GzEncoder::new(Vec::new(), Compression::default());
        let mut tar = Builder::new(gz);
        put_file(
            &mut tar,
            "oversize/SKILL.md",
            b"---\nname: oversize\ndescription: bomb test\n---\n",
        );
        // 1 MiB of zeros, repeated until we exceed the total decompressed
        // ceiling. Zeros compress to a few KiB, so the test archive stays
        // small while the decompressed stream blows past the cap.
        let chunk = vec![0_u8; 1024 * 1024];
        let chunks = (MAX_TOTAL_EXTRACTED_BYTES / chunk.len() as u64) + 4;
        for index in 0..chunks {
            put_file(
                &mut tar,
                &format!("oversize/references/pad-{index:04}.bin"),
                &chunk,
            );
        }
        tar.finish().unwrap();
        let bytes = finish(tar);

        let err = read_skill_md_entry(&bytes)
            .err()
            .expect("archive must be rejected");
        let message = format!("{err:?}");
        assert!(
            message.contains("limit")
                || message.contains("extract")
                || message.contains("archive entry"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn gzip_bomb_without_valid_tar_headers_fails_within_cap() {
        // Raw zeros large enough that an unbounded decoder would happily
        // produce more than the ceiling. With the cap in place, the tar
        // parser surfaces a validation error rather than burning the budget.
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        let chunk = vec![0_u8; 1024 * 1024];
        for _ in 0..((MAX_TOTAL_EXTRACTED_BYTES / chunk.len() as u64) + 4) {
            encoder.write_all(&chunk).unwrap();
        }
        let bytes = encoder.finish().unwrap();

        let err = read_skill_md_entry(&bytes)
            .err()
            .expect("malformed archive must be rejected");
        let _ = format!("{err:?}");
    }
}
