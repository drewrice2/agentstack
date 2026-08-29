//! Skill model: SKILL.md parsing, rendering, and the typed surfaces the rest
//! of the CLI works against.
//!
//! - [`parse_skill_md`] turns text into a [`ParsedSkillMd`] (fields may be
//!   missing — parsing is permissive).
//! - [`validate::validate_skill`] enforces the hard rules and produces typed
//!   [`ValidationError`]s.
//! - [`lint::lint_skill`] runs the soft rules and produces typed
//!   [`LintWarning`]s.
//! - [`inspect::inspect_skill`] aggregates both plus a directory walk into a
//!   stable, JSON-serializable [`SkillInspection`].

mod discover;
mod inspect;
mod lint;
mod validate;

pub use discover::{DiscoveredSkill, discover_skills};
pub use inspect::{DirectoryReport, DirectorySummary, SkillInspection, SkillMdInfo, inspect_skill};
pub use lint::{LintCode, LintConfig, LintWarning, lint_skill};
pub(crate) use validate::validate_skill_with_expected_dir_name;
pub use validate::{Position, ValidationCode, ValidationError, ValidationOutcome, validate_skill};

use serde::{Deserialize, Serialize};

use crate::error::SkillError;

/// Manifest filename inside a skill directory.
pub const SKILL_MD: &str = "SKILL.md";

/// Subdirectories a well-formed skill is expected to contain.
pub const STANDARD_SUBDIRS: &[&str] = &["references", "examples", "assets", "scripts", "platform"];

/// H1 sections recommended in SKILL.md. Missing ones produce lint warnings.
pub const REQUIRED_SECTIONS: &[&str] = &[
    "Purpose",
    "When to Use",
    "Instructions",
    "Output",
    "Boundaries",
];

/// Hard cap on the description length. Anything longer is a validation error.
pub const MAX_DESCRIPTION_LEN: usize = 500;

/// Hard cap on the skill name length.
pub const MAX_NAME_LEN: usize = 64;

/// Default soft character limit for SKILL.md when none is supplied.
pub const DEFAULT_SOFT_CHAR_LIMIT: usize = 8000;

/// Permissive view of YAML frontmatter — both fields are optional so the
/// validator can produce a precise error code when one is missing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RawManifest {
    pub name: Option<String>,
    pub description: Option<String>,
}

/// Validated frontmatter — both required fields present and well-formed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
}

/// Raw parse result for a SKILL.md document. No policy applied beyond YAML
/// syntax — fields can be missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillMd {
    pub raw_manifest: RawManifest,
    /// Original YAML frontmatter text (without the `---` delimiters).
    pub frontmatter_text: String,
    /// Titles of `# H1` sections, in document order.
    pub sections: Vec<String>,
    /// Original markdown body (everything after the closing `---`).
    pub body: String,
    /// Total character count of the original SKILL.md content.
    pub char_count: usize,
}

/// Parse a SKILL.md document. Performs no I/O. Empty/missing manifest fields
/// are not a parse error — they surface later as [`ValidationError`]s.
pub fn parse_skill_md(content: &str) -> Result<ParsedSkillMd, SkillError> {
    let (yaml, body) = split_frontmatter(content)?;
    let raw_manifest: RawManifest = if yaml.trim().is_empty() {
        RawManifest::default()
    } else {
        serde_yaml::from_str(yaml).map_err(|e| {
            let location = e.location().map(|loc| crate::error::SourcePosition {
                // The parsed YAML starts one line after the opening delimiter.
                line: loc.line() + 1,
                col: loc.column(),
            });
            SkillError::InvalidFrontmatter {
                message: e.to_string(),
                location,
            }
        })?
    };
    let sections = parse_h1_sections(body);
    Ok(ParsedSkillMd {
        raw_manifest,
        frontmatter_text: yaml.to_string(),
        sections,
        body: body.to_string(),
        char_count: content.chars().count(),
    })
}

/// Render a SKILL.md skeleton for a freshly-scaffolded skill.
pub fn render_skill_md(manifest: &SkillManifest) -> Result<String, SkillError> {
    let yaml = serde_yaml::to_string(manifest).map_err(|e| SkillError::InvalidFrontmatter {
        message: e.to_string(),
        location: None,
    })?;
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(yaml.trim_end());
    out.push_str("\n---\n\n");
    out.push_str("# Purpose\n\n");
    out.push_str(&format!(
        "Help an agent respond well when this trigger applies: {}.\n\n",
        manifest.description
    ));
    out.push_str("# When to Use\n\n");
    // Keep the "Use when" lead-in, but avoid doubling it when the description is
    // already phrased that way (e.g. "Use when reviewing PRs").
    let trigger = manifest
        .description
        .strip_prefix("Use when ")
        .unwrap_or(&manifest.description);
    out.push_str(&format!("Use when {trigger}.\n\n"));
    out.push_str("# Instructions\n\n");
    out.push_str("1. Read the user's request and identify the outcome they need.\n");
    out.push_str("2. Gather only the context needed for that outcome.\n");
    out.push_str("3. Apply this skill's guidance and keep the response focused.\n");
    out.push_str("4. State any assumptions, blockers, or follow-up actions clearly.\n\n");
    out.push_str("# Output\n\n");
    out.push_str(
        "Return a concise, actionable response with enough detail for the user to continue.\n\n",
    );
    out.push_str("# Boundaries\n\n");
    out.push_str("Do not invent missing facts, expose secrets, or continue when the task requires user approval.\n");
    Ok(out)
}

/// Reusable slug check. Returns the reason a name is invalid, or `Ok(())`.
pub fn check_slug(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("name must not be empty".into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("name must be at most {MAX_NAME_LEN} characters"));
    }
    let first = name.chars().next().expect("non-empty");
    if !first.is_ascii_lowercase() {
        return Err("name must start with a lowercase ASCII letter".into());
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err("only lowercase letters, digits, and hyphens are allowed".into());
        }
    }
    if name.contains("--") {
        return Err("consecutive hyphens are not allowed".into());
    }
    if name.ends_with('-') {
        return Err("name must not end with a hyphen".into());
    }
    Ok(())
}

/// Reusable description sanity check (used by `init`). The full hard rule
/// set is in [`validate`].
pub fn check_description(description: &str) -> Result<(), String> {
    let trimmed = description.trim();
    if trimmed.is_empty() {
        return Err("description must not be empty".into());
    }
    if trimmed.chars().count() > MAX_DESCRIPTION_LEN {
        return Err(format!(
            "description must be at most {MAX_DESCRIPTION_LEN} characters"
        ));
    }
    if trimmed.contains('\n') {
        return Err("description must be a single line".into());
    }
    Ok(())
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), SkillError> {
    // Strip a leading UTF-8 BOM if present.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);

    let first_nl = content.find('\n').ok_or(SkillError::MissingFrontmatter)?;
    if content[..first_nl].trim_end_matches('\r') != "---" {
        return Err(SkillError::MissingFrontmatter);
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
            let yaml = content[yaml_start..cursor].trim_end_matches(['\n', '\r']);
            let body_start = (line_end + 1).min(content.len());
            return Ok((yaml, &content[body_start..]));
        }
        if line_end == content.len() {
            break;
        }
        cursor = line_end + 1;
    }
    Err(SkillError::InvalidFrontmatter {
        message: "no closing `---` delimiter".into(),
        location: Some(crate::error::SourcePosition { line: 2, col: 1 }),
    })
}

fn parse_h1_sections(body: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        if line.starts_with('\t') || line.chars().take_while(|&c| c == ' ').count() >= 4 {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some((fence_char, fence_len)) = fence {
            let closing_len = trimmed.chars().take_while(|&c| c == fence_char).count();
            if closing_len >= fence_len && trimmed[closing_len..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        let fence_char = trimmed.chars().next();
        if matches!(fence_char, Some('`' | '~')) {
            let fence_char = fence_char.unwrap();
            let fence_len = trimmed.chars().take_while(|&c| c == fence_char).count();
            if fence_len >= 3 {
                fence = Some((fence_char, fence_len));
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let title = rest.trim().to_string();
            if !title.is_empty() {
                sections.push(title);
            }
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rules() {
        assert!(check_slug("my-skill").is_ok());
        assert!(check_slug("a").is_ok());
        assert!(check_slug("ab1").is_ok());
        assert!(check_slug("a-b-c").is_ok());

        assert!(check_slug("").is_err());
        assert!(check_slug("Code-review").is_err());
        assert!(check_slug("1leading").is_err());
        assert!(check_slug("trail-").is_err());
        assert!(check_slug("double--hyphen").is_err());
        assert!(check_slug("with space").is_err());
        assert!(check_slug("under_score").is_err());
        assert!(check_slug("-leading").is_err());
        let too_long: String = "a".repeat(MAX_NAME_LEN + 1);
        assert!(check_slug(&too_long).is_err());
    }

    #[test]
    fn description_rules() {
        assert!(check_description("does X when Y").is_ok());
        assert!(check_description("").is_err());
        assert!(check_description("   ").is_err());
        assert!(check_description("line1\nline2").is_err());
        let big: String = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(check_description(&big).is_err());
    }

    #[test]
    fn parse_minimal_skill_md() {
        let content = "\
---
name: my-skill
description: triggers when foo
---

# Purpose

Some text.

# When to Use

# Instructions

# Output

# Boundaries
";
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.raw_manifest.name.as_deref(), Some("my-skill"));
        assert_eq!(
            parsed.raw_manifest.description.as_deref(),
            Some("triggers when foo")
        );
        assert_eq!(
            parsed.sections,
            vec![
                "Purpose",
                "When to Use",
                "Instructions",
                "Output",
                "Boundaries"
            ]
        );
        assert!(parsed.char_count > 0);
        assert!(parsed.body.contains("# Purpose"));
    }

    #[test]
    fn parse_allows_missing_fields() {
        let content = "---\nname: only-name\n---\n\n# Purpose\n";
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.raw_manifest.name.as_deref(), Some("only-name"));
        assert!(parsed.raw_manifest.description.is_none());
    }

    #[test]
    fn parse_rejects_missing_frontmatter() {
        let err = parse_skill_md("# Purpose\n").unwrap_err();
        assert!(matches!(err, SkillError::MissingFrontmatter));
    }

    #[test]
    fn parse_rejects_unterminated_frontmatter() {
        let err = parse_skill_md("---\nname: x\ndescription: y\n").unwrap_err();
        assert!(matches!(err, SkillError::InvalidFrontmatter { .. }));
    }

    #[test]
    fn parse_rejects_malformed_yaml() {
        let err = parse_skill_md("---\nname: : :\n---\n").unwrap_err();
        assert!(matches!(err, SkillError::InvalidFrontmatter { .. }));
    }

    #[test]
    fn parse_handles_crlf() {
        let content = "---\r\nname: my-skill\r\ndescription: trig\r\n---\r\n\r\n# Purpose\r\n";
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.raw_manifest.name.as_deref(), Some("my-skill"));
        assert_eq!(parsed.sections, vec!["Purpose"]);
    }

    #[test]
    fn parse_strips_bom() {
        let content = "\u{feff}---\nname: x\ndescription: y\n---\n\n# Purpose\n";
        let parsed = parse_skill_md(content).unwrap();
        assert_eq!(parsed.raw_manifest.name.as_deref(), Some("x"));
    }

    #[test]
    fn parse_preserves_body() {
        let content = "---\nname: x\ndescription: y\n---\n\n# Purpose\n\nHello world.\n";
        let parsed = parse_skill_md(content).unwrap();
        assert!(parsed.body.contains("Hello world."));
    }

    #[test]
    fn render_round_trips_through_parse() {
        let manifest = SkillManifest {
            name: "my-skill".into(),
            description: "Use when X happens: a thing".into(),
        };
        let rendered = render_skill_md(&manifest).unwrap();
        let parsed = parse_skill_md(&rendered).unwrap();
        assert_eq!(parsed.raw_manifest.name.as_deref(), Some("my-skill"));
        assert_eq!(
            parsed.raw_manifest.description.as_deref(),
            Some("Use when X happens: a thing")
        );
        for required in REQUIRED_SECTIONS {
            assert!(parsed.sections.iter().any(|s| s == required));
        }
    }

    #[test]
    fn h1_section_parsing_skips_code_fences() {
        let body = "\
# Purpose

```
# fake heading inside fence
```

# Instructions
";
        let sections = parse_h1_sections(body);
        assert_eq!(sections, vec!["Purpose", "Instructions"]);
    }

    #[test]
    fn h1_section_parsing_skips_tilde_code_fences() {
        let body = "\
# Purpose

~~~
# fake heading inside fence
~~~

# Instructions
";
        let sections = parse_h1_sections(body);
        assert_eq!(sections, vec!["Purpose", "Instructions"]);
    }

    #[test]
    fn h1_section_parsing_skips_indented_code_blocks() {
        let body = "\
# Purpose

    # fake heading inside indented code

# Instructions
";
        let sections = parse_h1_sections(body);
        assert_eq!(sections, vec!["Purpose", "Instructions"]);
    }

    #[test]
    fn h1_section_parsing_skips_mixed_code_fences() {
        let body = "\
# Purpose

```
# fake heading inside backtick fence
```

# Instructions

~~~~
# fake heading inside tilde fence
~~~~

# Output
";
        let sections = parse_h1_sections(body);
        assert_eq!(sections, vec!["Purpose", "Instructions", "Output"]);
    }
}
