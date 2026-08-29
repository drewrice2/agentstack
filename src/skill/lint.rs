//! Soft "should-do" rules for skill quality.
//!
//! Failures here become [`LintWarning`]s, not validation errors. Authors
//! should usually fix them, but the format technically remains valid.

use std::path::Path;

use serde::Serialize;

use super::{DEFAULT_SOFT_CHAR_LIMIT, ParsedSkillMd};

/// Stable, snake_case lint code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LintCode {
    PlaceholderContent,
    VagueDescription,
    NonTriggerDescription,
    MissingSectionPurpose,
    MissingSectionWhenToUse,
    MissingSectionInstructions,
    MissingSectionOutput,
    MissingSectionBoundaries,
    NoExamplesDirectory,
    NoReferencesDirectory,
    SkillMdTooLong,
    UnreferencedReference,
}

impl LintCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            LintCode::PlaceholderContent => "placeholder_content",
            LintCode::VagueDescription => "vague_description",
            LintCode::NonTriggerDescription => "non_trigger_description",
            LintCode::MissingSectionPurpose => "missing_section_purpose",
            LintCode::MissingSectionWhenToUse => "missing_section_when_to_use",
            LintCode::MissingSectionInstructions => "missing_section_instructions",
            LintCode::MissingSectionOutput => "missing_section_output",
            LintCode::MissingSectionBoundaries => "missing_section_boundaries",
            LintCode::NoExamplesDirectory => "no_examples_directory",
            LintCode::NoReferencesDirectory => "no_references_directory",
            LintCode::SkillMdTooLong => "skill_md_too_long",
            LintCode::UnreferencedReference => "unreferenced_reference",
        }
    }
}

impl std::fmt::Display for LintCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single lint finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LintWarning {
    pub code: LintCode,
    pub message: String,
}

impl std::fmt::Display for LintWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

/// Configurable knobs for the lint rules.
#[derive(Debug, Clone)]
pub struct LintConfig {
    /// Soft character limit for SKILL.md. Files longer than this trigger
    /// [`LintCode::SkillMdTooLong`].
    pub soft_char_limit: usize,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self {
            soft_char_limit: DEFAULT_SOFT_CHAR_LIMIT,
        }
    }
}

/// Recommended H1 sections and the lint code emitted when each is missing.
/// The section names mirror [`crate::skill::REQUIRED_SECTIONS`].
const MISSING_SECTION_LINTS: &[(&str, LintCode)] = &[
    ("Purpose", LintCode::MissingSectionPurpose),
    ("When to Use", LintCode::MissingSectionWhenToUse),
    ("Instructions", LintCode::MissingSectionInstructions),
    ("Output", LintCode::MissingSectionOutput),
    ("Boundaries", LintCode::MissingSectionBoundaries),
];

/// Phrases a trigger-oriented description is allowed to start with.
const TRIGGER_PREFIXES: &[&str] = &[
    "use when",
    "use for",
    "use to",
    "triggers when",
    "triggered when",
    "when ",
];

/// Run every soft rule against an already-validated skill.
///
/// `content` is the raw SKILL.md text — used to check whether reference
/// files are mentioned in the document.
pub fn lint_skill(
    root: &Path,
    parsed: &ParsedSkillMd,
    content: &str,
    config: &LintConfig,
) -> Vec<LintWarning> {
    let mut warnings = Vec::new();

    if let Some(marker) = find_placeholder_marker(content) {
        warnings.push(LintWarning {
            code: LintCode::PlaceholderContent,
            message: format!("SKILL.md still contains a placeholder marker (`{marker}`)"),
        });
    }

    if let Some(description) = parsed.raw_manifest.description.as_deref() {
        let trimmed = description.trim();
        if !trimmed.is_empty() {
            if is_vague(trimmed) {
                warnings.push(LintWarning {
                    code: LintCode::VagueDescription,
                    message: format!(
                        "description looks vague (`{trimmed}`); aim for a specific trigger"
                    ),
                });
            }
            if !is_trigger_oriented(trimmed) {
                warnings.push(LintWarning {
                    code: LintCode::NonTriggerDescription,
                    message:
                        "description should start with `Use when...`, `Use for...`, `Use to...`, \
                         or similar trigger phrasing"
                            .into(),
                });
            }
        }
    }

    for (section, code) in MISSING_SECTION_LINTS {
        if !parsed.sections.iter().any(|s| s == section) {
            warnings.push(LintWarning {
                code: *code,
                message: format!("missing recommended section `# {section}`"),
            });
        }
    }

    if !root.join("examples").is_dir() {
        warnings.push(LintWarning {
            code: LintCode::NoExamplesDirectory,
            message: "no `examples/` directory; add concrete examples to anchor the skill".into(),
        });
    }
    if !root.join("references").is_dir() {
        warnings.push(LintWarning {
            code: LintCode::NoReferencesDirectory,
            message: "no `references/` directory; add reference material the agent can cite".into(),
        });
    }

    if parsed.char_count > config.soft_char_limit {
        warnings.push(LintWarning {
            code: LintCode::SkillMdTooLong,
            message: format!(
                "SKILL.md is {} characters; soft limit is {}",
                parsed.char_count, config.soft_char_limit
            ),
        });
    }

    let references_dir = root.join("references");
    if references_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&references_dir)
    {
        let mut names: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if !entry.path().is_file() {
                continue;
            }
            names.push(name);
        }
        names.sort();
        for name in names {
            if !content.contains(&format!("references/{name}")) {
                warnings.push(LintWarning {
                    code: LintCode::UnreferencedReference,
                    message: format!("`references/{name}` is not mentioned in SKILL.md"),
                });
            }
        }
    }

    warnings
}

/// Bare scaffold/placeholder words. Matched case-insensitively as whole
/// words so prose like "AUTOTODO" or "fixmeup" does not trip the rule.
const PLACEHOLDER_WORDS: &[&str] = &["TODO", "FIXME", "XXX"];

/// Whether a byte is part of an identifier word (letters, digits, `_`).
/// Used for the word-boundary check around placeholder words.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Look for a leftover placeholder marker in SKILL.md.
///
/// Catches two shapes authors actually leave behind:
/// - whole-word scaffold markers `TODO`, `FIXME`, `XXX` (case-insensitive),
///   ignoring matches glued into larger words; and
/// - angle-bracket scaffold tokens such as `<placeholder>`, `<your name>`,
///   or `<...>` that the author forgot to fill in.
///
/// Returns the offending marker (as found) for an actionable message, or
/// `None` when the content is clean. Designed for low false positives:
/// real prose rarely contains `<lowercase-words>` tokens, and the bare-word
/// matches require word boundaries.
fn find_placeholder_marker(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();

    for word in PLACEHOLDER_WORDS {
        let needle = word.to_ascii_lowercase();
        let mut from = 0;
        while let Some(rel) = lower[from..].find(&needle) {
            let start = from + rel;
            let end = start + needle.len();
            let before_ok = start == 0 || !is_word_byte(lower_bytes[start - 1]);
            let after_ok = end == lower_bytes.len() || !is_word_byte(lower_bytes[end]);
            if before_ok && after_ok {
                // Report the marker as it appears in the source.
                return Some(content[start..end].to_string());
            }
            from = start + 1;
        }
    }

    if let Some(token) = find_angle_placeholder(content) {
        return Some(token);
    }

    None
}

/// Detect an angle-bracket placeholder token like `<placeholder>`,
/// `<your name>`, or `<...>`. We only treat a `<...>` span as a placeholder
/// when its inner text looks like a scaffold slot (lowercase words, spaces,
/// hyphens, underscores, or `...`) rather than HTML/markup such as `<br>` or
/// `<https://example.com>`. This keeps false positives low.
fn find_angle_placeholder(content: &str) -> Option<String> {
    let mut search = 0;
    while let Some(rel) = content[search..].find('<') {
        let open = search + rel;
        if let Some(rel_close) = content[open + 1..].find('>') {
            let close = open + 1 + rel_close;
            let inner = &content[open + 1..close];
            if is_placeholder_slot(inner) {
                return Some(content[open..=close].to_string());
            }
            search = close + 1;
        } else {
            break;
        }
    }
    None
}

/// Whether the text inside `<...>` looks like an unfilled scaffold slot.
///
/// `inner` is the raw text between the angle brackets (not trimmed). A real
/// scaffold token has no padding, so a `<...>` span produced incidentally by
/// prose like `a < b and c > d` (inner `" b and c "`, note the surrounding
/// spaces) is rejected up front — this is the key false-positive guard.
fn is_placeholder_slot(inner: &str) -> bool {
    if inner.is_empty() {
        return false;
    }
    // Incidental `<...>` spans from comparisons carry padding spaces; real
    // scaffold tokens never do.
    if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
        return false;
    }
    // A literal ellipsis slot, e.g. `<...>`.
    if inner == "..." {
        return true;
    }
    // Reject anything with characters that suggest real markup/URLs/code
    // (slashes, colons, dots, equals, quotes, uppercase tags, etc.).
    let allowed = inner
        .chars()
        .all(|c| c.is_ascii_lowercase() || c == ' ' || c == '-' || c == '_');
    if !allowed {
        return false;
    }
    // Require a known scaffold lead-in word. We deliberately do NOT treat any
    // multi-word lowercase span as a placeholder, since that over-matches
    // ordinary prose caught between `<` and `>`.
    const LEAD_INS: &[&str] = &["your", "placeholder", "insert", "describe", "name"];
    let first_word = inner.split([' ', '-', '_']).next().unwrap_or("");
    LEAD_INS.contains(&first_word)
}

/// Heuristic for "this description doesn't say much". Keeps the bar low so
/// it doesn't fire on real one-liners.
fn is_vague(description: &str) -> bool {
    description.split_whitespace().count() < 4
}

/// Whether the description begins with a recognized trigger phrase.
fn is_trigger_oriented(description: &str) -> bool {
    let lower = description.to_ascii_lowercase();
    TRIGGER_PREFIXES.iter().any(|p| lower.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed_clean() -> ParsedSkillMd {
        ParsedSkillMd {
            raw_manifest: crate::skill::RawManifest {
                name: Some("demo".into()),
                description: Some("Use when testing placeholder lint".into()),
            },
            frontmatter_text: String::new(),
            sections: vec![
                "Purpose".into(),
                "When to Use".into(),
                "Instructions".into(),
                "Output".into(),
                "Boundaries".into(),
            ],
            body: String::new(),
            char_count: 0,
        }
    }

    fn has_placeholder(content: &str) -> bool {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("references")).unwrap();
        std::fs::create_dir(root.join("examples")).unwrap();
        lint_skill(root, &parsed_clean(), content, &LintConfig::default())
            .iter()
            .any(|w| w.code == LintCode::PlaceholderContent)
    }

    #[test]
    fn placeholder_flags_fixme() {
        assert!(has_placeholder("Fill this in. FIXME before publishing."));
    }

    #[test]
    fn placeholder_flags_bare_todo() {
        assert!(has_placeholder("Step 1. TODO write the rest of the steps."));
    }

    #[test]
    fn placeholder_flags_legacy_todo_colon() {
        assert!(has_placeholder("TODO: replace this section"));
    }

    #[test]
    fn placeholder_flags_xxx_and_angle_tokens() {
        assert!(has_placeholder("Set the key to XXX here."));
        assert!(has_placeholder(
            "Use the value <your api key> when calling."
        ));
        assert!(has_placeholder("Replace <placeholder> with the real text."));
        assert!(has_placeholder("Describe the flow: <...>"));
    }

    #[test]
    fn placeholder_ignores_clean_realistic_body() {
        let body = "# Purpose\n\nHelp the agent triage incoming support tickets and route them \
            to the right team. Read the ticket, classify the intent, and propose a next action. \
            Cross-reference the customer's plan tier before escalating. See the comparison of \
            x and y, where x < y, for prioritization. Email the summary to ops@example.com.\n";
        assert!(!has_placeholder(body));
    }

    #[test]
    fn placeholder_ignores_words_containing_markers() {
        // Whole-word boundaries: these must NOT trip.
        assert!(!has_placeholder(
            "The autotodo system and a fixmeup helper run nightly."
        ));
        assert!(!has_placeholder(
            "Inspect the xxxl size chart and the todolist module."
        ));
    }

    #[test]
    fn placeholder_ignores_real_markup_and_urls() {
        assert!(!has_placeholder(
            "Use a <br> tag and link <https://example.com> for context."
        ));
        assert!(!has_placeholder(
            "Compare values where a < b and c > d in the formula."
        ));
    }

    #[test]
    fn placeholder_marker_is_named_in_message() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("references")).unwrap();
        std::fs::create_dir(root.join("examples")).unwrap();
        let warnings = lint_skill(
            root,
            &parsed_clean(),
            "Finish the FIXME note.",
            &LintConfig::default(),
        );
        let msg = warnings
            .iter()
            .find(|w| w.code == LintCode::PlaceholderContent)
            .map(|w| w.message.clone())
            .unwrap();
        assert!(
            msg.contains("FIXME"),
            "message should name the marker: {msg}"
        );
    }

    #[test]
    fn vague_detector() {
        assert!(is_vague("a thing"));
        assert!(is_vague("does stuff"));
        assert!(!is_vague("Use when authentication tokens expire"));
    }

    #[test]
    fn trigger_detector() {
        assert!(is_trigger_oriented("Use when X happens"));
        assert!(is_trigger_oriented("use to refactor a module"));
        assert!(is_trigger_oriented("Triggers when foo"));
        assert!(is_trigger_oriented("When the agent must do X"));
        assert!(!is_trigger_oriented("Refactors a module"));
        assert!(!is_trigger_oriented("Helpers for things"));
    }

    #[test]
    fn missing_section_lints_match_required_sections() {
        let names: Vec<&str> = MISSING_SECTION_LINTS
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(names.as_slice(), crate::skill::REQUIRED_SECTIONS);
    }

    #[test]
    fn code_snake_case_matches_serde_output() {
        let codes = [
            LintCode::PlaceholderContent,
            LintCode::VagueDescription,
            LintCode::NonTriggerDescription,
            LintCode::MissingSectionPurpose,
            LintCode::MissingSectionWhenToUse,
            LintCode::MissingSectionInstructions,
            LintCode::MissingSectionOutput,
            LintCode::MissingSectionBoundaries,
            LintCode::NoExamplesDirectory,
            LintCode::NoReferencesDirectory,
            LintCode::SkillMdTooLong,
            LintCode::UnreferencedReference,
        ];
        for code in codes {
            let serde_str = serde_json::to_string(&code).unwrap();
            let serde_str = serde_str.trim_matches('"');
            assert_eq!(serde_str, code.as_str(), "mismatch for {code:?}");
        }
    }

    #[test]
    fn unreferenced_reference_requires_references_path() {
        let tmp = assert_fs::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("references")).unwrap();
        std::fs::create_dir(root.join("examples")).unwrap();
        std::fs::write(root.join("references").join("auth.md"), "auth reference").unwrap();
        let parsed = ParsedSkillMd {
            raw_manifest: crate::skill::RawManifest {
                name: Some("demo".into()),
                description: Some("Use when testing reference lint".into()),
            },
            frontmatter_text: String::new(),
            sections: vec![
                "Purpose".into(),
                "When to Use".into(),
                "Instructions".into(),
                "Output".into(),
                "Boundaries".into(),
            ],
            body: String::new(),
            char_count: 0,
        };

        let warnings = lint_skill(
            root,
            &parsed,
            "The author wrote notes, but no reference path.",
            &LintConfig::default(),
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.code == LintCode::UnreferencedReference)
        );

        let warnings = lint_skill(
            root,
            &parsed,
            "See references/auth.md for details.",
            &LintConfig::default(),
        );
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.code == LintCode::UnreferencedReference)
        );
    }
}
