//! Parsing and rendering for fully-qualified skill references — the canonical
//! way to name a skill in the registry, receipts, and API calls.
//!
//! Two forms:
//!
//! - `org/skill` — the current approved version
//! - `org/skill@version` — a specific version
//!
//! CLI commands may accept org-relative inputs such as `skill` or
//! `skill@version`, but those are resolved through token org context before a
//! [`SkillRef`] is built.
//!
//! Both `org` and `skill` use the same slug rules as a skill name (lowercase
//! ASCII letters, digits, hyphens; must start with a letter; no consecutive
//! hyphens; ≤ [`MAX_NAME_LEN`] chars). Versions are looser — see
//! [`check_version`] — because the registry is the authority on which
//! versions actually exist; the CLI only enforces shape.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

#[cfg(test)]
use crate::skill::MAX_NAME_LEN;
use crate::skill::check_slug;

/// Maximum length for a version segment. Mirrors [`MAX_NAME_LEN`] so common
/// shapes like SemVer build metadata fit easily.
pub const MAX_VERSION_LEN: usize = 64;

/// Errors produced while validating a version segment.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("version must not be empty")]
    Empty,
    #[error("version must be at most {max} characters")]
    TooLong { max: usize },
    #[error("version `{version}` must not contain whitespace")]
    ContainsWhitespace { version: String },
    #[error("version `{version}` must not contain `@` or `/`")]
    ContainsSeparator { version: String },
}

/// Errors produced while parsing or constructing a skill reference.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillRefError {
    #[error("skill ref must not be empty")]
    Empty,
    #[error("skill ref `{input}` must not have leading or trailing whitespace")]
    SurroundingWhitespace { input: String },
    #[error("skill ref `{input}` must be in the form `org/skill` or `org/skill@version`")]
    InvalidForm { input: String },
    #[error("skill ref `{input}` must contain exactly one `/` between org and skill")]
    TooManySlashes { input: String },
    #[error("invalid org `{org}`: {reason}")]
    InvalidOrg { org: String, reason: String },
    #[error("invalid skill name `{name}`: {reason}")]
    InvalidSkillName { name: String, reason: String },
    #[error(transparent)]
    Version(#[from] VersionError),
}

/// A parsed reference to a remote skill.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillRef {
    pub org: String,
    pub name: String,
    pub version: Option<String>,
}

impl SkillRef {
    /// Parse `org/skill` or `org/skill@version`. Whitespace around the input
    /// is rejected so refs read identically wherever they appear.
    pub fn parse(input: &str) -> Result<Self, SkillRefError> {
        if input.is_empty() {
            return Err(SkillRefError::Empty);
        }
        if input.trim() != input {
            return Err(SkillRefError::SurroundingWhitespace {
                input: input.to_string(),
            });
        }

        let (org_skill, version) = match input.split_once('@') {
            Some((left, right)) => (left, Some(right)),
            None => (input, None),
        };

        let (org, name) = match org_skill.split_once('/') {
            Some((o, n)) => (o, n),
            None => {
                return Err(SkillRefError::InvalidForm {
                    input: input.to_string(),
                });
            }
        };

        if org_skill.matches('/').count() > 1 {
            return Err(SkillRefError::TooManySlashes {
                input: input.to_string(),
            });
        }

        check_slug(org).map_err(|reason| SkillRefError::InvalidOrg {
            org: org.to_string(),
            reason,
        })?;
        check_slug(name).map_err(|reason| SkillRefError::InvalidSkillName {
            name: name.to_string(),
            reason,
        })?;

        let version = match version {
            Some(v) => Some(check_version(v)?.to_string()),
            None => None,
        };

        Ok(Self {
            org: org.to_string(),
            name: name.to_string(),
            version,
        })
    }

    /// Build a ref without a version.
    pub fn new(org: impl Into<String>, name: impl Into<String>) -> Result<Self, SkillRefError> {
        let org = org.into();
        let name = name.into();
        check_slug(&org).map_err(|reason| SkillRefError::InvalidOrg {
            org: org.clone(),
            reason,
        })?;
        check_slug(&name).map_err(|reason| SkillRefError::InvalidSkillName {
            name: name.clone(),
            reason,
        })?;
        Ok(Self {
            org,
            name,
            version: None,
        })
    }

    /// Return a copy of this ref pinned to the given version.
    pub fn with_version(mut self, version: impl Into<String>) -> Result<Self, VersionError> {
        let v = version.into();
        check_version(&v)?;
        self.version = Some(v);
        Ok(self)
    }

    /// `org/skill` form, dropping any version.
    pub fn unversioned(&self) -> String {
        format!("{}/{}", self.org, self.name)
    }
}

impl fmt::Display for SkillRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(v) => write!(f, "{}/{}@{v}", self.org, self.name),
            None => write!(f, "{}/{}", self.org, self.name),
        }
    }
}

impl FromStr for SkillRef {
    type Err = SkillRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Verify a version string is shaped sensibly. The registry is authoritative
/// on which versions exist; the CLI only enforces:
///
/// - non-empty;
/// - ≤ [`MAX_VERSION_LEN`] chars;
/// - no whitespace, no `@`, no `/`.
pub fn check_version(version: &str) -> Result<&str, VersionError> {
    if version.is_empty() {
        return Err(VersionError::Empty);
    }
    if version.chars().count() > MAX_VERSION_LEN {
        return Err(VersionError::TooLong {
            max: MAX_VERSION_LEN,
        });
    }
    for c in version.chars() {
        if c.is_whitespace() {
            return Err(VersionError::ContainsWhitespace {
                version: version.to_string(),
            });
        }
        if c == '@' || c == '/' {
            return Err(VersionError::ContainsSeparator {
                version: version.to_string(),
            });
        }
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unversioned_ref() {
        let r = SkillRef::parse("acme/code-review").unwrap();
        assert_eq!(r.org, "acme");
        assert_eq!(r.name, "code-review");
        assert!(r.version.is_none());
        assert_eq!(r.to_string(), "acme/code-review");
        assert_eq!(r.unversioned(), "acme/code-review");
    }

    #[test]
    fn parses_versioned_ref() {
        let r = SkillRef::parse("acme/code-review@1.2.3").unwrap();
        assert_eq!(r.org, "acme");
        assert_eq!(r.name, "code-review");
        assert_eq!(r.version.as_deref(), Some("1.2.3"));
        assert_eq!(r.to_string(), "acme/code-review@1.2.3");
        assert_eq!(r.unversioned(), "acme/code-review");
    }

    #[test]
    fn parses_loose_versions() {
        // The registry is the source of truth on what versions exist; the
        // CLI just needs to confirm the shape is sane. SemVer pre-release,
        // build metadata, and content-hash-style tags should all parse.
        for v in [
            "1.0.0",
            "v1.2.3",
            "1.2.3-beta.1",
            "1.2.3+build.5",
            "2026-05-06",
            "abc123def456",
        ] {
            let r = SkillRef::parse(&format!("acme/x@{v}")).unwrap();
            assert_eq!(r.version.as_deref(), Some(v));
        }
    }

    #[test]
    fn rejects_blank_input() {
        assert_eq!(SkillRef::parse("").unwrap_err(), SkillRefError::Empty);

        let err = SkillRef::parse(" acme/x").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::SurroundingWhitespace { input } if input == " acme/x"
        ));
        assert!(err.to_string().contains("leading or trailing whitespace"));

        let err = SkillRef::parse("acme/x ").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::SurroundingWhitespace { input } if input == "acme/x "
        ));
        assert!(err.to_string().contains("leading or trailing whitespace"));
    }

    #[test]
    fn rejects_missing_slash() {
        let err = SkillRef::parse("acme").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::InvalidForm { input } if input == "acme"
        ));
        assert!(err.to_string().contains("must be in the form `org/skill`"));
    }

    #[test]
    fn rejects_too_many_slashes() {
        let err = SkillRef::parse("a/b/c").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::TooManySlashes { input } if input == "a/b/c"
        ));
        assert!(err.to_string().contains("exactly one `/`"));
    }

    #[test]
    fn rejects_bad_org_slug() {
        let err = SkillRef::parse("ACME/x").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::InvalidOrg { org, reason }
                if org == "ACME" && reason.contains("lowercase ASCII letter")
        ));
        assert!(err.to_string().contains("invalid org"));
    }

    #[test]
    fn rejects_bad_skill_slug() {
        let err = SkillRef::parse("acme/Bad_Name").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::InvalidSkillName { name, reason }
                if name == "Bad_Name" && reason.contains("lowercase ASCII letter")
        ));
        assert!(err.to_string().contains("invalid skill name"));
    }

    #[test]
    fn rejects_empty_version() {
        let err = SkillRef::parse("acme/x@").unwrap_err();
        assert!(matches!(&err, SkillRefError::Version(VersionError::Empty)));
        assert!(err.to_string().contains("version must not be empty"));
    }

    #[test]
    fn rejects_double_at() {
        // `acme/x@1@2` splits into ("acme/x", "1@2"); 1@2 has an `@` which
        // should fail version validation.
        let err = SkillRef::parse("acme/x@1@2").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::Version(VersionError::ContainsSeparator { version })
                if version == "1@2"
        ));
        assert!(err.to_string().contains("must not contain `@`"));
    }

    #[test]
    fn rejects_whitespace_in_version() {
        let err = SkillRef::parse("acme/x@1 0 0").unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::Version(VersionError::ContainsWhitespace { version })
                if version == "1 0 0"
        ));
        assert!(err.to_string().contains("whitespace"));
    }

    #[test]
    fn from_str_works() {
        let r: SkillRef = "acme/x@1".parse().unwrap();
        assert_eq!(r.version.as_deref(), Some("1"));
    }

    #[test]
    fn from_str_returns_typed_error() {
        let err = "acme".parse::<SkillRef>().unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::InvalidForm { input } if input == "acme"
        ));
    }

    #[test]
    fn check_slug_max_len_is_respected_for_org_and_name() {
        let big: String = "a".repeat(MAX_NAME_LEN + 1);
        let err = SkillRef::parse(&format!("{big}/skill")).unwrap_err();
        assert!(matches!(&err, SkillRefError::InvalidOrg { .. }));

        let err = SkillRef::parse(&format!("acme/{big}")).unwrap_err();
        assert!(matches!(&err, SkillRefError::InvalidSkillName { .. }));
    }

    #[test]
    fn version_len_is_capped() {
        let big: String = "1".repeat(MAX_VERSION_LEN + 1);
        let err = SkillRef::parse(&format!("acme/x@{big}")).unwrap_err();
        assert!(matches!(
            &err,
            SkillRefError::Version(VersionError::TooLong { max }) if *max == MAX_VERSION_LEN
        ));
        assert!(err.to_string().contains("at most"));
    }

    #[test]
    fn check_version_returns_typed_errors() {
        let err = check_version("").unwrap_err();
        assert_eq!(err, VersionError::Empty);
        assert_eq!(err.to_string(), "version must not be empty");

        let err = check_version("bad/version").unwrap_err();
        assert!(matches!(
            &err,
            VersionError::ContainsSeparator { version } if version == "bad/version"
        ));
        assert!(err.to_string().contains("must not contain `@` or `/`"));
    }

    #[test]
    fn constructors_return_typed_errors() {
        let err = SkillRef::new("ACME", "x").unwrap_err();
        assert!(matches!(&err, SkillRefError::InvalidOrg { .. }));

        let err = SkillRef::new("acme", "Bad_Name").unwrap_err();
        assert!(matches!(&err, SkillRefError::InvalidSkillName { .. }));

        let r = SkillRef::new("acme", "x").unwrap();
        let err = r.with_version("bad/version").unwrap_err();
        assert!(matches!(
            &err,
            VersionError::ContainsSeparator { version } if version == "bad/version"
        ));
    }

    #[test]
    fn with_version_round_trips() {
        let r = SkillRef::new("acme", "x").unwrap();
        let r = r.with_version("0.1.0").unwrap();
        assert_eq!(r.to_string(), "acme/x@0.1.0");
    }
}
