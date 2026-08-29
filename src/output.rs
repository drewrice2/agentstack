//! Process-wide output context and CLI error rendering.
//!
//! `Ctx` carries global output and prompting flags down into command handlers
//! so they all decide consistently whether to print human or machine output,
//! whether to chatter on stderr, whether prompting is allowed, and whether to
//! suppress non-essential success messages.

use std::io::{self, BufRead, IsTerminal, Write};

use crate::cli::GlobalArgs;
use crate::error::CliError;
use crate::skill_ref::{SkillRefError, VersionError};
use anyhow::{Result, bail};
use clap::error::{Error as ClapError, ErrorKind};
use serde_json::{Map, Value, json};

/// Process-wide output flags. Constructed once from [`GlobalArgs`] and passed
/// to every command handler.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ctx {
    pub json: bool,
    pub no_input: bool,
    pub verbose: bool,
    pub quiet: bool,
}

impl Ctx {
    pub fn from_global(g: &GlobalArgs) -> Self {
        Self {
            json: g.json,
            no_input: g.no_input,
            verbose: g.verbose,
            quiet: g.quiet,
        }
    }

    /// True when commands must not ask follow-up questions.
    pub fn no_input(&self) -> bool {
        self.no_input || self.json || env_noninteractive()
    }

    /// True when it is safe to prompt the user.
    pub fn can_prompt(&self) -> bool {
        prompt_allowed(
            self.no_input(),
            self.quiet,
            io::stdin().is_terminal(),
            io::stdout().is_terminal(),
            io::stderr().is_terminal(),
        )
    }

    /// Ask for a yes/no confirmation on stderr. Refuses when prompting is not
    /// allowed so non-interactive callers never hang.
    pub fn prompt_confirm(
        &self,
        prompt: impl AsRef<str>,
        noninteractive_message: impl AsRef<str>,
    ) -> Result<bool> {
        if !self.can_prompt() {
            bail!("{}", noninteractive_message.as_ref());
        }
        eprint!("{} [y/N] ", prompt.as_ref());
        if io::stderr().flush().is_err() {
            return Ok(false);
        }
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        Ok(matches!(
            line.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    /// Print to stdout unless `--quiet` is set. JSON output is emitted by
    /// handlers directly; this is for the human path.
    pub fn say(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            println!("{}", msg.as_ref());
        }
    }

    /// Like `say`, but always — used for primary command results that the
    /// user explicitly asked for (e.g. `cache path`, `targets path`).
    pub fn say_always(&self, msg: impl AsRef<str>) {
        println!("{}", msg.as_ref());
    }

    /// Verbose-only diagnostic line. Goes to stderr so it never pollutes a
    /// JSON document on stdout.
    pub fn verbose(&self, msg: impl AsRef<str>) {
        if self.verbose && !self.json {
            let _ = writeln!(io::stderr(), "[verbose] {}", msg.as_ref());
        }
    }

    /// Non-fatal diagnostic line. Goes to stderr and respects `--quiet`.
    pub fn warn(&self, msg: impl AsRef<str>) {
        if !self.quiet {
            let _ = writeln!(io::stderr(), "{}", msg.as_ref());
        }
    }
}

fn env_noninteractive() -> bool {
    env_truthy("AGENTSTACK_NONINTERACTIVE") || env_truthy("CI")
}

fn prompt_allowed(
    no_input: bool,
    quiet: bool,
    stdin_tty: bool,
    stdout_tty: bool,
    stderr_tty: bool,
) -> bool {
    !no_input && !quiet && stdin_tty && stdout_tty && stderr_tty
}

fn env_truthy(name: &str) -> bool {
    let Some(value) = std::env::var_os(name) else {
        return false;
    };
    let value = value.to_string_lossy();
    let value = value.trim();
    !value.is_empty()
        && !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
}

/// Render a top-level error to stderr in the format we want.
///
/// Plain text:
///
/// ```text
/// error: <message>
///   caused by: <cause-1>
///   caused by: <cause-2>
/// ```
///
/// JSON (when `--json` is set):
///
/// ```text
/// {"error":{"code":"<code>","message":"<message>","causes":["<cause-1>"]}}
/// ```
pub fn render_error(err: &anyhow::Error, json: bool) {
    if json {
        let lock = err
            .downcast_ref::<crate::install::TargetBusyError>()
            .map(|e| {
                serde_json::json!({
                    "target_path": e.target_root,
                    "lock_path": e.lock_path,
                    "lock_age_seconds": e.lock_age.map(|age| age.as_secs()),
                    "pid": e.pid,
                    "hostname": e.hostname,
                    "suggested_next_command": e.suggested_next_command,
                })
            });
        let structured = structured_error(err);
        let fallback_message;
        let primary_message = match structured.as_ref() {
            Some(error) => error.message.as_str(),
            None => {
                fallback_message = err.to_string();
                fallback_message.as_str()
            }
        };
        let causes = json_causes(err, primary_message);
        let mut body = Map::new();
        body.insert(
            "code".to_string(),
            json!(
                structured
                    .as_ref()
                    .map(|e| e.code.as_str())
                    .unwrap_or(if lock.is_some() {
                        "target_busy"
                    } else {
                        "command_failed"
                    })
            ),
        );
        if let Some(error) = structured.as_ref() {
            body.insert("message".to_string(), json!(error.message.as_str()));
            if let Some(resource) = error.resource.as_deref() {
                body.insert("resource".to_string(), json!(resource));
            }
            if let Some(action) = error.action.as_deref() {
                body.insert("action".to_string(), json!(action));
            }
            if let Some(status) = error.status.as_deref() {
                body.insert("status".to_string(), json!(status));
            }
            if let Some(http_status) = error.http_status {
                body.insert("http_status".to_string(), json!(http_status));
            }
            if let Some(next_command) = error.next_command.as_deref() {
                if is_concrete_next_command(next_command) {
                    body.insert("next_command".to_string(), json!(next_command));
                } else if is_template_next_command(next_command) {
                    body.insert("next_command_template".to_string(), json!(next_command));
                }
            }
            if let Some(machine_hint) = error.machine_hint.as_deref() {
                body.insert("machine_hint".to_string(), json!(machine_hint));
            }
            if !error.auth_methods.is_empty() {
                body.insert("auth_methods".to_string(), json!(error.auth_methods));
            }
        } else {
            body.insert("message".to_string(), json!(err.to_string()));
        }
        body.insert("causes".to_string(), json!(causes));
        if let Some(lock) = lock {
            if !body.contains_key("next_command")
                && let Some(next_command) = lock
                    .get("suggested_next_command")
                    .and_then(serde_json::Value::as_str)
                    .filter(|command| is_concrete_next_command(command))
            {
                body.insert("next_command".to_string(), json!(next_command));
            }
            body.insert("lock".to_string(), lock);
        } else {
            body.insert("lock".to_string(), Value::Null);
        }
        let payload = json!({ "error": body });
        let _ = writeln!(io::stderr(), "{}", payload);
        return;
    }
    let _ = writeln!(io::stderr(), "error: {err}");
    for cause in err.chain().skip(1) {
        let _ = writeln!(io::stderr(), "  caused by: {cause}");
    }
    if let Some(next_command) = human_next_command(err) {
        let _ = writeln!(io::stderr(), "next: {next_command}");
    }
}

pub fn compact_human_text(value: &str, max_chars: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut clipped = normalized
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    clipped = clipped.trim_end().to_string();
    clipped.push_str("...");
    clipped
}

fn json_causes(err: &anyhow::Error, primary_message: &str) -> Vec<String> {
    let mut causes = Vec::new();
    for cause in err.chain().skip(1).map(|c| c.to_string()) {
        if cause == primary_message || causes.iter().any(|seen| seen == &cause) {
            continue;
        }
        causes.push(cause);
    }
    causes
}

pub(crate) fn is_concrete_next_command(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|first| first == "agentstack")
        && !(command.contains('<') || command.contains('>'))
}

fn is_template_next_command(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|first| first == "agentstack")
        && (command.contains('<') || command.contains('>'))
}

pub fn render_clap_error_json(err: &ClapError) {
    let payload = json!({
        "error": {
            "code": clap_error_code(err.kind()),
            "message": err.to_string(),
            "causes": [],
            "lock": Value::Null,
        }
    });
    let _ = writeln!(io::stderr(), "{payload}");
}

fn clap_error_code(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::MissingRequiredArgument
        | ErrorKind::MissingSubcommand
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => "usage_error",
        _ => "argument_error",
    }
}

fn structured_error(err: &anyhow::Error) -> Option<CliError> {
    for cause in err.chain() {
        if let Some(cli) = cause.downcast_ref::<CliError>() {
            return Some(cli.clone());
        }
        if let Some(skill_ref) = cause.downcast_ref::<SkillRefError>() {
            return Some(skill_ref_error(skill_ref));
        }
        if let Some(version) = cause.downcast_ref::<VersionError>() {
            return Some(CliError::new("invalid_version", version.to_string()));
        }
    }
    infer_error(err)
}

fn human_next_command(err: &anyhow::Error) -> Option<String> {
    if err
        .downcast_ref::<crate::install::TargetBusyError>()
        .is_some()
    {
        return None;
    }
    structured_error(err).and_then(|error| error.next_command)
}

fn skill_ref_error(err: &SkillRefError) -> CliError {
    let resource = match err {
        SkillRefError::Empty => None,
        SkillRefError::SurroundingWhitespace { input }
        | SkillRefError::InvalidForm { input }
        | SkillRefError::TooManySlashes { input } => Some(input.as_str()),
        SkillRefError::InvalidOrg { org, .. } => Some(org.as_str()),
        SkillRefError::InvalidSkillName { name, .. } => Some(name.as_str()),
        SkillRefError::Version(_) => None,
    };
    let mut error = CliError::new("invalid_skill_ref", err.to_string())
        .action("parse_skill_ref")
        .next_command("agentstack skill search <query>");
    if let Some(resource) = resource {
        error = error.resource(resource);
    }
    error
}

fn infer_error(err: &anyhow::Error) -> Option<CliError> {
    let message = err.to_string();
    if err
        .chain()
        .any(|cause| cause.to_string().contains("registry request failed"))
    {
        return Some(
            CliError::new("registry_unavailable", message)
                .action("registry_request")
                .next_command("agentstack registry ping"),
        );
    }
    if let Some(resource) = extract_backticked_after(&message, "invalid --org `")
        .or_else(|| extract_backticked_after(&message, "invalid org `"))
    {
        return Some(
            CliError::new("invalid_org", message)
                .resource(resource)
                .action("validate_org"),
        );
    }
    if let Some(resource) = extract_backticked_after(&message, "no install receipt for `") {
        return Some(
            CliError::new("install_receipt_missing", message)
                .resource(resource)
                .action("update")
                .next_command("agentstack install list --target <target>"),
        );
    }
    if let Some(resource) = extract_backticked_after(&message, "no stack install receipt for `") {
        return Some(
            CliError::new("install_receipt_missing", message)
                .resource(resource)
                .action("update")
                .next_command("agentstack install list --kind stack --target <target>"),
        );
    }
    None
}

fn extract_backticked_after(message: &str, marker: &str) -> Option<String> {
    let rest = message.split_once(marker)?.1;
    let value = rest.split_once('`')?.0;
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::{is_concrete_next_command, is_template_next_command, prompt_allowed};

    #[test]
    fn prompt_policy_requires_stdout_terminal() {
        assert!(prompt_allowed(false, false, true, true, true));
        assert!(!prompt_allowed(false, false, true, false, true));
    }

    #[test]
    fn prompt_policy_keeps_existing_noninteractive_gates() {
        assert!(!prompt_allowed(true, false, true, true, true));
        assert!(!prompt_allowed(false, true, true, true, true));
        assert!(!prompt_allowed(false, false, false, true, true));
        assert!(!prompt_allowed(false, false, true, true, false));
    }

    #[test]
    fn next_command_classifier_splits_concrete_and_template_guidance() {
        assert!(is_concrete_next_command(
            "agentstack skill install code-review --target local"
        ));
        assert!(!is_template_next_command(
            "agentstack skill install code-review --target local"
        ));

        assert!(!is_concrete_next_command(
            "agentstack skill install <skill> --target local"
        ));
        assert!(is_template_next_command(
            "agentstack skill install <skill> --target local"
        ));

        assert!(!is_concrete_next_command(
            "rerun the same command with --force"
        ));
        assert!(!is_template_next_command(
            "rerun the same command with --force"
        ));
    }
}
