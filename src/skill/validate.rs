//! Hard validation rules.
//!
//! Every rule in this file is binding: a failure should make
//! `agentstack skill validate` exit non-zero. Soft "should-do" rules live in
//! [`super::lint`].

use std::path::Path;

use serde::Serialize;

use super::{
    MAX_DESCRIPTION_LEN, ParsedSkillMd, SKILL_MD, SkillManifest, check_slug, parse_skill_md,
};
use crate::error::{SkillError, SourcePosition};

/// Stable, snake_case error code attached to every [`ValidationError`].
///
/// The `Serialize` impl produces these as snake_case strings so the JSON
/// emitted by `agentstack skill inspect --json` is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCode {
    NotADirectory,
    MissingSkillMd,
    InvalidUtf8,
    MissingFrontmatter,
    InvalidFrontmatter,
    MissingName,
    MissingDescription,
    InvalidName,
    NameMismatch,
    DescriptionTooLong,
    DescriptionMultiline,
    UnsupportedTopLevelEntry,
    IoError,
}

impl ValidationCode {
    /// Snake_case form. Mirrors what serde emits — kept manual so callers
    /// have a stable, allocation-free string for log lines.
    pub const fn as_str(self) -> &'static str {
        match self {
            ValidationCode::NotADirectory => "not_a_directory",
            ValidationCode::MissingSkillMd => "missing_skill_md",
            ValidationCode::InvalidUtf8 => "invalid_utf8",
            ValidationCode::MissingFrontmatter => "missing_frontmatter",
            ValidationCode::InvalidFrontmatter => "invalid_frontmatter",
            ValidationCode::MissingName => "missing_name",
            ValidationCode::MissingDescription => "missing_description",
            ValidationCode::InvalidName => "invalid_name",
            ValidationCode::NameMismatch => "name_mismatch",
            ValidationCode::DescriptionTooLong => "description_too_long",
            ValidationCode::DescriptionMultiline => "description_multiline",
            ValidationCode::UnsupportedTopLevelEntry => "unsupported_top_level_entry",
            ValidationCode::IoError => "io_error",
        }
    }
}

impl std::fmt::Display for ValidationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 1-based source position inside SKILL.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl From<SourcePosition> for Position {
    fn from(value: SourcePosition) -> Self {
        Self {
            line: value.line,
            col: value.col,
        }
    }
}

/// A single hard validation finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidationError {
    pub code: ValidationCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
}

impl ValidationError {
    fn new(code: ValidationCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            position: None,
        }
    }

    fn with_position(code: ValidationCode, message: impl Into<String>, position: Position) -> Self {
        Self {
            code,
            message: message.into(),
            position: Some(position),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.position {
            Some(position) => write!(
                f,
                "SKILL.md:{}:{}: error[{}]: {}",
                position.line, position.col, self.code, self.message
            ),
            None => write!(f, "SKILL.md: error[{}]: {}", self.code, self.message),
        }
    }
}

/// Output of [`validate_skill`]. Carries the parsed content forward so a
/// follow-on lint pass doesn't have to read the file again.
#[derive(Debug, Default)]
pub struct ValidationOutcome {
    pub errors: Vec<ValidationError>,
    pub content: Option<String>,
    pub parsed: Option<ParsedSkillMd>,
}

impl ValidationOutcome {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// If validation passed, extract a [`SkillManifest`].
    pub fn manifest(&self) -> Option<SkillManifest> {
        if !self.is_ok() {
            return None;
        }
        let parsed = self.parsed.as_ref()?;
        Some(SkillManifest {
            name: parsed.raw_manifest.name.clone()?,
            description: parsed.raw_manifest.description.clone()?,
        })
    }
}

/// Walk every hard rule and accumulate the failures. Stops at the first I/O
/// or parse failure that makes downstream checks meaningless (no SKILL.md,
/// invalid UTF-8, malformed frontmatter), but otherwise collects everything.
pub fn validate_skill(root: &Path) -> ValidationOutcome {
    validate_skill_with_expected_dir_name(root, None)
}

pub(crate) fn validate_skill_with_expected_dir_name(
    root: &Path,
    expected_dir_name: Option<&str>,
) -> ValidationOutcome {
    let mut errors = Vec::new();

    if !root.exists() || !root.is_dir() {
        errors.push(ValidationError::new(
            ValidationCode::NotADirectory,
            format!("`{}` is not a directory", root.display()),
        ));
        return ValidationOutcome {
            errors,
            ..Default::default()
        };
    }

    let manifest_path = root.join(SKILL_MD);
    let bytes = match std::fs::read(&manifest_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            errors.push(ValidationError::new(
                ValidationCode::MissingSkillMd,
                format!("missing SKILL.md at `{}`", manifest_path.display()),
            ));
            return ValidationOutcome {
                errors,
                ..Default::default()
            };
        }
        Err(e) => {
            errors.push(ValidationError::new(
                ValidationCode::IoError,
                format!("failed to read `{}`: {e}", manifest_path.display()),
            ));
            return ValidationOutcome {
                errors,
                ..Default::default()
            };
        }
    };

    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => {
            errors.push(ValidationError::new(
                ValidationCode::InvalidUtf8,
                format!("`{}` is not valid UTF-8", manifest_path.display()),
            ));
            return ValidationOutcome {
                errors,
                ..Default::default()
            };
        }
    };

    let parsed = match parse_skill_md(&content) {
        Ok(p) => p,
        Err(SkillError::MissingFrontmatter) => {
            errors.push(ValidationError::with_position(
                ValidationCode::MissingFrontmatter,
                "SKILL.md is missing YAML frontmatter (expected `---` delimiters)",
                top_of_file(),
            ));
            return ValidationOutcome {
                errors,
                content: Some(content),
                parsed: None,
            };
        }
        Err(SkillError::InvalidFrontmatter { message, location }) => {
            let mut err = ValidationError::new(
                ValidationCode::InvalidFrontmatter,
                format!("malformed YAML frontmatter: {message}"),
            );
            err.position = location.map(Position::from);
            errors.push(err);
            return ValidationOutcome {
                errors,
                content: Some(content),
                parsed: None,
            };
        }
    };

    let raw_name = parsed.raw_manifest.name.as_deref().unwrap_or("").trim();
    let raw_description = parsed
        .raw_manifest
        .description
        .as_deref()
        .unwrap_or("")
        .trim();
    let name_position =
        frontmatter_key_position(&parsed.frontmatter_text, "name").unwrap_or_else(top_of_file);
    let description_position = frontmatter_key_position(&parsed.frontmatter_text, "description")
        .unwrap_or_else(top_of_file);

    if raw_name.is_empty() {
        errors.push(ValidationError::with_position(
            ValidationCode::MissingName,
            "frontmatter field `name` is missing or empty",
            top_of_file(),
        ));
    } else if let Err(reason) = check_slug(raw_name) {
        errors.push(ValidationError::with_position(
            ValidationCode::InvalidName,
            format!("invalid skill name `{raw_name}`: {reason}"),
            name_position,
        ));
    } else {
        let expected_name = expected_dir_name.map(str::to_string).or_else(|| {
            std::fs::canonicalize(root)
                .ok()
                .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        });
        if let Some(dir_name) = expected_name
            && raw_name != dir_name
        {
            errors.push(ValidationError::with_position(
                    ValidationCode::NameMismatch,
                    format!(
                        "frontmatter field `name` must match directory name `{dir_name}` (got `{raw_name}`)"
                    ),
                    name_position,
                ));
        }
    }

    if raw_description.is_empty() {
        errors.push(ValidationError::with_position(
            ValidationCode::MissingDescription,
            "frontmatter field `description` is missing or empty",
            top_of_file(),
        ));
    } else {
        if raw_description.contains('\n') {
            errors.push(ValidationError::with_position(
                ValidationCode::DescriptionMultiline,
                "description must be a single line (it contains line breaks)",
                description_position,
            ));
        }
        let len = raw_description.chars().count();
        if len > MAX_DESCRIPTION_LEN {
            errors.push(ValidationError::with_position(
                ValidationCode::DescriptionTooLong,
                format!("description is {len} characters; the limit is {MAX_DESCRIPTION_LEN}"),
                description_position,
            ));
        }
    }

    for entry in unsupported_top_level_entries(root) {
        errors.push(ValidationError::new(
            ValidationCode::UnsupportedTopLevelEntry,
            format!(
                "unsupported top-level entry `{entry}`; skills may contain only regular files and directories"
            ),
        ));
    }

    ValidationOutcome {
        errors,
        content: Some(content),
        parsed: Some(parsed),
    }
}

/// Any visible regular file or directory is supported skill content
/// (excluded names are skipped at pack time). Symlinks and other special
/// entries cannot be packaged, so they fail validation.
fn unsupported_top_level_entries(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(file_type) = entry.file_type() else {
            out.push(name);
            continue;
        };
        if file_type.is_dir() || file_type.is_file() {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out
}

fn top_of_file() -> Position {
    Position { line: 1, col: 1 }
}

/// Locate the 1-based file position where a top-level YAML key appears in the
/// frontmatter block. Returns None if not found.
fn frontmatter_key_position(frontmatter: &str, key: &str) -> Option<Position> {
    for (line_idx, line) in frontmatter.lines().enumerate() {
        let col = line.chars().take_while(|c| c.is_whitespace()).count() + 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line_has_key(trimmed, key) {
            return Some(Position {
                line: line_idx + 2,
                col,
            });
        }
    }
    None
}

fn line_has_key(trimmed: &str, key: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(key) else {
        return quoted_line_has_key(trimmed, key);
    };
    rest.trim_start().starts_with(':')
}

fn quoted_line_has_key(trimmed: &str, key: &str) -> bool {
    for quote in ['"', '\''] {
        let Some(rest) = trimmed.strip_prefix(quote) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(quote) else {
            continue;
        };
        if rest.trim_start().starts_with(':') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::prelude::*;

    fn write_skill(dir: &assert_fs::fixture::ChildPath, content: &str) {
        dir.create_dir_all().unwrap();
        dir.child(SKILL_MD).write_str(content).unwrap();
    }

    fn error_for(outcome: &ValidationOutcome, code: ValidationCode) -> &ValidationError {
        outcome
            .errors
            .iter()
            .find(|err| err.code == code)
            .unwrap_or_else(|| panic!("missing {code:?} in {:?}", outcome.errors))
    }

    #[test]
    fn code_snake_case_matches_serde_output() {
        let codes = [
            ValidationCode::NotADirectory,
            ValidationCode::MissingSkillMd,
            ValidationCode::InvalidUtf8,
            ValidationCode::MissingFrontmatter,
            ValidationCode::InvalidFrontmatter,
            ValidationCode::MissingName,
            ValidationCode::MissingDescription,
            ValidationCode::InvalidName,
            ValidationCode::NameMismatch,
            ValidationCode::DescriptionTooLong,
            ValidationCode::DescriptionMultiline,
            ValidationCode::UnsupportedTopLevelEntry,
            ValidationCode::IoError,
        ];
        for code in codes {
            let serde_str = serde_json::to_string(&code).unwrap();
            // Strip the surrounding quotes that JSON adds.
            let serde_str = serde_str.trim_matches('"');
            assert_eq!(serde_str, code.as_str(), "mismatch for {code:?}");
        }
    }

    #[test]
    fn position_for_name_mismatch_points_at_name_line() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(
            &target,
            "---\nname: wrong-name\ndescription: Use when foo\n---\n",
        );

        let outcome = validate_skill(target.path());
        let err = error_for(&outcome, ValidationCode::NameMismatch);
        assert_eq!(err.position, Some(Position { line: 2, col: 1 }));
    }

    #[test]
    fn position_for_description_too_long() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        let long = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        write_skill(
            &target,
            &format!("---\nname: demo\ndescription: {long}\n---\n"),
        );

        let outcome = validate_skill(target.path());
        let err = error_for(&outcome, ValidationCode::DescriptionTooLong);
        assert_eq!(err.position, Some(Position { line: 3, col: 1 }));
    }

    #[test]
    fn multiline_description_fails_with_position() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(
            &target,
            "---\nname: demo\ndescription: |-\n  line one\n  line two\n---\n",
        );

        let outcome = validate_skill(target.path());
        assert_eq!(outcome.errors.len(), 1, "errors: {:?}", outcome.errors);
        let err = error_for(&outcome, ValidationCode::DescriptionMultiline);
        assert_eq!(err.position, Some(Position { line: 3, col: 1 }));
    }

    #[test]
    fn folded_description_collapsing_to_single_line_passes() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(
            &target,
            "---\nname: demo\ndescription: >-\n  Use when foo\n  and bar\n---\n",
        );

        let outcome = validate_skill(target.path());
        assert!(outcome.is_ok(), "errors: {:?}", outcome.errors);
    }

    #[test]
    fn position_for_missing_frontmatter_is_top_of_file() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(&target, "# Purpose\n");

        let outcome = validate_skill(target.path());
        let err = error_for(&outcome, ValidationCode::MissingFrontmatter);
        assert_eq!(err.position, Some(Position { line: 1, col: 1 }));
    }

    #[test]
    fn position_for_invalid_frontmatter_uses_parser_location_when_available() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("bad-yaml");
        let yaml = "name: : :\n";
        write_skill(&target, &format!("---\n{yaml}---\n"));

        let parser_has_location = serde_yaml::from_str::<serde_yaml::Value>(yaml)
            .unwrap_err()
            .location()
            .is_some();
        let outcome = validate_skill(target.path());
        let err = error_for(&outcome, ValidationCode::InvalidFrontmatter);
        assert_eq!(err.position.is_some(), parser_has_location);
    }

    #[test]
    fn support_files_and_directories_pass_validation() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(&target, "---\nname: demo\ndescription: Use when foo\n---\n");
        target.child("reference.md").write_str("reference").unwrap();
        target.child("LICENSE.txt").write_str("license").unwrap();
        target.child("templates/form.md").write_str("form").unwrap();
        target
            .child("agents/openai.yaml")
            .write_str("name: demo")
            .unwrap();
        target
            .child("python/helper.py")
            .write_str("print('ok')")
            .unwrap();

        let outcome = validate_skill(target.path());
        assert!(outcome.is_ok(), "unexpected errors: {:?}", outcome.errors);
    }

    #[cfg(unix)]
    #[test]
    fn position_omitted_for_file_level_errors() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let target = tmp.child("demo");
        write_skill(&target, "---\nname: demo\ndescription: Use when foo\n---\n");
        std::os::unix::fs::symlink("SKILL.md", target.child("link.md").path()).unwrap();

        let outcome = validate_skill(target.path());
        let err = error_for(&outcome, ValidationCode::UnsupportedTopLevelEntry);
        assert_eq!(err.position, None);
    }
}
