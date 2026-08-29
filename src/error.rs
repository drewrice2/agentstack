use thiserror::Error;

/// Structured metadata attached to user-facing CLI failures.
///
/// The `Display` text remains the human error message; JSON rendering may use
/// the optional fields as an automation contract.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct CliError {
    pub code: String,
    pub message: String,
    pub resource: Option<String>,
    pub action: Option<String>,
    pub status: Option<String>,
    pub http_status: Option<u16>,
    pub next_command: Option<String>,
    pub machine_hint: Option<String>,
    pub auth_methods: Vec<String>,
}

impl CliError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            resource: None,
            action: None,
            status: None,
            http_status: None,
            next_command: None,
            machine_hint: None,
            auth_methods: Vec::new(),
        }
    }

    pub fn resource(mut self, resource: impl Into<String>) -> Self {
        self.resource = Some(resource.into());
        self
    }

    pub fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    pub fn status(mut self, status: impl Into<String>) -> Self {
        self.status = Some(status.into());
        self
    }

    pub fn http_status(mut self, http_status: u16) -> Self {
        self.http_status = Some(http_status);
        self
    }

    pub fn next_command(mut self, next_command: impl Into<String>) -> Self {
        self.next_command = Some(next_command.into());
        self
    }

    pub fn machine_hint(mut self, machine_hint: impl Into<String>) -> Self {
        self.machine_hint = Some(machine_hint.into());
        self
    }

    pub fn auth_methods(
        mut self,
        auth_methods: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.auth_methods = auth_methods.into_iter().map(Into::into).collect();
        self
    }
}

/// 1-based source position reported by lower-level parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePosition {
    pub line: usize,
    pub col: usize,
}

/// Errors raised by the SKILL.md parser. Higher-level commands and the
/// validator translate these into typed [`ValidationError`] records.
///
/// [`ValidationError`]: crate::skill::ValidationError
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("missing YAML frontmatter (expected `---` block at top of SKILL.md)")]
    MissingFrontmatter,

    #[error("malformed YAML frontmatter: {message}")]
    InvalidFrontmatter {
        message: String,
        location: Option<SourcePosition>,
    },
}
