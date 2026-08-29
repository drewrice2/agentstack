//! Skill inspection: aggregate validation, lint, and a filesystem walk into
//! a single, JSON-serializable summary.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{
    LintConfig, LintWarning, SKILL_MD, STANDARD_SUBDIRS, ValidationError, lint_skill,
    validate_skill,
};
use crate::package::hash_skill_package;

/// Stable JSON-serializable view of a skill directory.
///
/// The shape of this struct (field names + types + presence) is part of the
/// public CLI contract — every field is always emitted, with `null` when
/// not applicable, so downstream consumers can rely on it.
#[derive(Debug, Clone, Serialize)]
pub struct SkillInspection {
    pub name: Option<String>,
    pub description: Option<String>,
    pub path: PathBuf,
    pub skill_md: Option<SkillMdInfo>,
    pub directories: DirectoryReport,
    pub unknown_files: Vec<String>,
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<LintWarning>,
    /// SHA-256 hex of the deterministic package archive when packaging
    /// succeeds. `null` when validation or packaging fails.
    pub package_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillMdInfo {
    pub char_count: usize,
    pub sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DirectoryReport {
    pub references: DirectorySummary,
    pub examples: DirectorySummary,
    pub assets: DirectorySummary,
    pub scripts: DirectorySummary,
    pub platform: DirectorySummary,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DirectorySummary {
    pub present: bool,
    pub files: Vec<String>,
}

/// Build a [`SkillInspection`] for `root`. Never fails: every problem
/// surfaces as a [`ValidationError`] or [`LintWarning`] inside the result.
pub fn inspect_skill(root: &Path, config: &LintConfig) -> SkillInspection {
    let outcome = validate_skill(root);
    let warnings = match (outcome.parsed.as_ref(), outcome.content.as_deref()) {
        (Some(parsed), Some(content)) => lint_skill(root, parsed, content, config),
        _ => Vec::new(),
    };

    let directories = build_directory_report(root);
    let unknown_files = collect_unknown_files(root);

    let (name, description, skill_md) = match outcome.parsed.as_ref() {
        Some(p) => (
            p.raw_manifest.name.clone(),
            p.raw_manifest.description.clone(),
            Some(SkillMdInfo {
                char_count: p.char_count,
                sections: p.sections.clone(),
            }),
        ),
        None => (None, None, None),
    };

    let package_hash = if outcome.is_ok() {
        hash_skill_package(root).ok().map(|hash| hash.hex)
    } else {
        None
    };

    SkillInspection {
        name,
        description,
        path: root.to_path_buf(),
        skill_md,
        directories,
        unknown_files,
        errors: outcome.errors,
        warnings,
        package_hash,
    }
}

fn build_directory_report(root: &Path) -> DirectoryReport {
    DirectoryReport {
        references: list_dir_files(&root.join("references")),
        examples: list_dir_files(&root.join("examples")),
        assets: list_dir_files(&root.join("assets")),
        scripts: list_dir_files(&root.join("scripts")),
        platform: list_dir_files(&root.join("platform")),
    }
}

fn list_dir_files(dir: &Path) -> DirectorySummary {
    if !dir.is_dir() {
        return DirectorySummary {
            present: false,
            files: Vec::new(),
        };
    }
    let mut files = collect_files_recursive(dir, dir);
    files.sort();
    DirectorySummary {
        present: true,
        files,
    }
}

fn collect_files_recursive(base: &Path, dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        // Use the entry's own file type so symlinks are not followed: a symlink
        // cycle would otherwise recurse until the stack overflows.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            out.extend(collect_files_recursive(base, &path));
        } else if file_type.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            out.push(path_display(rel));
        }
    }
    out
}

/// Files in `root` that are neither SKILL.md nor under a known subdirectory.
fn collect_unknown_files(root: &Path) -> Vec<String> {
    let known: HashSet<&str> = STANDARD_SUBDIRS.iter().copied().collect();
    let mut out = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == SKILL_MD {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if known.contains(name_str.as_ref()) {
                continue;
            }
            for f in collect_files_recursive(root, &path) {
                out.push(f);
            }
        } else if file_type.is_file() {
            out.push(name_str.into_owned());
        }
    }
    out.sort();
    out
}

/// Forward-slash path display so tests are stable across Windows.
fn path_display(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
