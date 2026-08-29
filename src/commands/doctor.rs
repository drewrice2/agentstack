//! `agentstack doctor` — diagnose the local install.
//!
//! Reports a list of `(name, status, detail)` rows. Status is one of:
//!
//! - `ok`    — the check passed. Missing config, cache, token, or default
//!   install paths are ok on a fresh local install.
//! - `warn`  — non-fatal: AgentStack can run, but something is worth
//!   noticing (e.g. a user-level target exists on disk but is not
//!   registered). Warns do not always prescribe a next command.
//! - `fail`  — a configured value is broken (e.g. an override path that
//!   doesn't exist and isn't writable)
//!
//! `doctor` itself exits 0 unless something raised. Tokens are never printed;
//! the report only shows whether one is present and where it came from.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use super::client::validate_resolved_registry_url;
use crate::cache::Cache;
use crate::config::{self, ConfigStore, REGISTRY_URL_ENV, RegistryUrlSource};
use crate::credentials::{
    ALLOW_TOKEN_FILE_ENV, DEFAULT_ACCOUNT, TOKEN_FILE_ENV, TOKEN_PATH_ENV, TokenSource,
    env_token_present, resolve_token, scoped_account, token_file_allowed, token_file_override,
    token_path_override, token_store,
};
use crate::output::Ctx;
use crate::receipt::RECEIPT_FILE;
use crate::targets::{InstallTarget, TargetResolver, parent_creatable, writable_dir};

/// Severity of a single check result.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub code: String,
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix_command: Option<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    cli_version: &'static str,
    checks: Vec<Check>,
    summary: Summary,
}

#[derive(Debug, Serialize)]
struct Summary {
    ok: usize,
    warn: usize,
    fail: usize,
}

pub fn run(ctx: &Ctx) -> Result<()> {
    let mut checks = Vec::<Check>::new();
    // Loaded once and shared; each section still reports its own check when
    // the config cannot be loaded.
    let store = ConfigStore::load();
    push_cli_version(&mut checks);
    push_config(&mut checks);
    push_cache(&mut checks);
    push_registry_and_auth(&mut checks, &store);
    push_targets(&mut checks, &store);
    push_installed_receipts(&mut checks, &store);

    let summary = Summary {
        ok: checks.iter().filter(|c| c.status == Status::Ok).count(),
        warn: checks.iter().filter(|c| c.status == Status::Warn).count(),
        fail: checks.iter().filter(|c| c.status == Status::Fail).count(),
    };

    if ctx.json {
        let report = Report {
            cli_version: env!("CARGO_PKG_VERSION"),
            checks,
            summary,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if !ctx.quiet {
        let name_w = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        let status_w = checks
            .iter()
            .map(|c| c.status.label().len())
            .max()
            .unwrap_or(0);
        for c in &checks {
            println!(
                "  [{status:<sw$}]  {name:<nw$}  {detail}",
                status = c.status.label(),
                name = c.name,
                detail = c.detail,
                sw = status_w,
                nw = name_w,
            );
        }
        println!();
        println!(
            "summary: {} ok, {} warn, {} fail",
            summary.ok, summary.warn, summary.fail
        );
        let fixes = next_actions(&checks);
        if !fixes.is_empty() {
            println!();
            println!("next:");
            for fix in fixes {
                println!("  {fix}");
            }
        }
    }
    Ok(())
}

fn push_cli_version(checks: &mut Vec<Check>) {
    checks.push(check(
        "cli_version",
        "cli version",
        Status::Ok,
        format!("agentstack {}", env!("CARGO_PKG_VERSION")),
        None,
    ));
}

fn push_config(checks: &mut Vec<Check>) {
    let config_dir_result = config::config_dir();
    match config_dir_result {
        Ok(dir) => {
            checks.push(check_dir_present_or_creatable(
                "config_dir",
                "config dir",
                &dir,
            ));
            let cfg_file = dir.join(config::CONFIG_FILE_NAME);
            if cfg_file.exists() {
                match ConfigStore::load_from(cfg_file.clone()) {
                    Ok(_) => checks.push(check(
                        "config_file",
                        "config file",
                        Status::Ok,
                        format!("{} (parses cleanly)", cfg_file.display()),
                        None,
                    )),
                    Err(e) => checks.push(check(
                        "config_file",
                        "config file",
                        Status::Fail,
                        format!("{}: {e}", cfg_file.display()),
                        Some("agentstack config show".to_string()),
                    )),
                }
            } else {
                checks.push(check(
                    "config_file",
                    "config file",
                    Status::Ok,
                    format!(
                        "{} (not yet created; local commands do not need it)",
                        cfg_file.display()
                    ),
                    None,
                ));
            }
        }
        Err(e) => checks.push(check(
            "config_dir",
            "config dir",
            Status::Fail,
            format!("{e}"),
            None,
        )),
    }
}

fn push_cache(checks: &mut Vec<Check>) {
    match config::cache_dir() {
        Ok(dir) => {
            checks.push(check_dir_present_or_creatable(
                "cache_dir",
                "cache dir",
                &dir,
            ));
            // Inspect cache contents without forcing creation.
            if dir.exists() {
                let cache = Cache::at(dir);
                match cache.list() {
                    Ok(entries) => checks.push(check(
                        "cache_contents",
                        "cache contents",
                        Status::Ok,
                        format!("{} cached package(s)", entries.len()),
                        None,
                    )),
                    Err(e) => checks.push(check(
                        "cache_contents",
                        "cache contents",
                        Status::Fail,
                        format!("{e}"),
                        Some("agentstack cache list".to_string()),
                    )),
                }
            }
        }
        Err(e) => checks.push(check(
            "cache_dir",
            "cache dir",
            Status::Fail,
            format!("{e}"),
            None,
        )),
    }
}

fn push_registry_and_auth(checks: &mut Vec<Check>, store: &Result<ConfigStore>) {
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            checks.push(check(
                "registry_url",
                "registry url",
                Status::Fail,
                format!("could not load config: {e}"),
                Some("agentstack config show".to_string()),
            ));
            return;
        }
    };
    let resolved = store.resolved_registry_url();
    let has_registry_url = match validate_resolved_registry_url(&resolved) {
        Ok(()) => {
            let detail = match resolved.source {
                RegistryUrlSource::Env => format!("{} (from {REGISTRY_URL_ENV})", resolved.url),
                RegistryUrlSource::Config => resolved.url.clone(),
                RegistryUrlSource::Default => format!("{} (default)", resolved.url),
            };
            checks.push(check(
                "registry_url",
                "registry url",
                Status::Ok,
                detail,
                None,
            ));
            true
        }
        Err(e) => {
            let fix = match resolved.source {
                RegistryUrlSource::Env => {
                    format!("unset {REGISTRY_URL_ENV} or set it to a valid URL")
                }
                RegistryUrlSource::Config | RegistryUrlSource::Default => {
                    "agentstack registry use <URL>".to_string()
                }
            };
            checks.push(check(
                "registry_url",
                "registry url",
                Status::Fail,
                e.to_string(),
                Some(fix),
            ));
            false
        }
    };

    let file_path = token_file_override();
    let file_allowed = file_path.is_some() && token_file_allowed();
    let backing_store = token_store();
    let token_account = scoped_account(&resolved.url, DEFAULT_ACCOUNT).ok();
    let resolved_token = match token_account.as_deref() {
        Some(account) => resolve_token(backing_store.as_ref(), account),
        None => Ok(None),
    };
    let auth_check = match resolved_token {
        Ok(Some((_tok, src))) => {
            let src_label = match src {
                TokenSource::Env => "AGENTSTACK_TOKEN env var",
                TokenSource::Path => "AGENTSTACK_TOKEN_PATH file",
                TokenSource::Store => backing_store.kind(),
            };
            check(
                "auth_token",
                "auth token",
                Status::Warn,
                format!("present (from {src_label}); run `agentstack auth whoami` to verify it"),
                Some("agentstack auth whoami".to_string()),
            )
        }
        Ok(None) => check(
            "auth_token",
            "auth token",
            Status::Ok,
            "not logged in; local commands do not need a token".to_string(),
            None,
        ),
        Err(e) => check(
            "auth_token",
            "auth token",
            Status::Fail,
            format!("could not resolve auth token: {e}"),
            Some(if has_registry_url {
                "agentstack auth login".to_string()
            } else {
                "agentstack registry use <URL> && agentstack auth login".to_string()
            }),
        ),
    };
    checks.push(auth_check);

    if env_token_present() {
        checks.push(check(
            "env_override",
            "env override",
            Status::Ok,
            "AGENTSTACK_TOKEN is set (overrides stored token)",
            None,
        ));
    }
    if let Some(path) = token_path_override() {
        checks.push(check(
            "env_token_path",
            "env token path",
            Status::Ok,
            format!("{TOKEN_PATH_ENV}={} (read-only token file)", path.display()),
            None,
        ));
    }
    if let Some(path) = file_path {
        let detail = if file_allowed {
            format!(
                "{TOKEN_FILE_ENV}={} (test-only plaintext store; do not use in production)",
                path.display()
            )
        } else {
            format!(
                "{TOKEN_FILE_ENV}={} (refused; set {ALLOW_TOKEN_FILE_ENV}=1 to opt in or unset {TOKEN_FILE_ENV})",
                path.display()
            )
        };
        let status = if file_allowed {
            Status::Warn
        } else {
            Status::Fail
        };
        let fix = if file_allowed {
            None
        } else {
            Some(format!("unset {TOKEN_FILE_ENV}"))
        };
        checks.push(check("token_file", "token file", status, detail, fix));
    }
}

fn push_targets(checks: &mut Vec<Check>, store: &Result<ConfigStore>) {
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            checks.push(check(
                "targets",
                "targets",
                Status::Fail,
                format!("could not load config: {e}"),
                Some("agentstack config show".to_string()),
            ));
            return;
        }
    };
    let resolver = TargetResolver::new(store);
    for row in doctor_targets()
        .into_iter()
        .map(|target| resolver.detect(target))
    {
        let name = format!("target: {}", row.target);
        let code = format!("target_{}", row.target.replace('-', "_"));
        let path = row
            .path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<missing>".to_string());
        let mut detail = format!("{path} ({})", row.source);
        let mut fix = None;
        let status = if !row.absolute {
            detail.push_str(" — not absolute");
            fix = row.fix_command.clone();
            Status::Fail
        } else if row.exists && !row.is_dir {
            detail.push_str(" — exists but is not a directory");
            fix = row.fix_command.clone();
            Status::Fail
        } else if !row.configured && user_level_target(row.target) && row.usable {
            // Opt-in targets. Warn only when the default dir already exists
            // so a clean home is not two-warns-for-nothing.
            detail.push_str(&format!(
                " — not configured; run `agentstack target setup {} --yes` before user-level installs",
                row.target
            ));
            if row.exists { Status::Warn } else { Status::Ok }
        } else if row.writable {
            detail.push_str(" — writable");
            Status::Ok
        } else if row.creatable {
            // Destination does not exist yet, but the parent is writable.
            // Configured overrides should still suggest setup. Default
            // local/repo paths are created on first install.
            if row.configured {
                detail.push_str(&format!(
                    " — not yet created (run `agentstack target setup {} --yes`)",
                    row.target
                ));
                fix = Some(format!("agentstack target setup {} --yes", row.target));
                Status::Warn
            } else {
                detail.push_str(" — created on first install");
                Status::Ok
            }
        } else {
            detail.push_str(" — missing; parent does not exist or is not writable");
            fix = row.fix_command.clone();
            Status::Fail
        };
        checks.push(check(code, name, status, detail, fix));
    }
}

fn user_level_target(target: &str) -> bool {
    matches!(target, "claude-code" | "codex")
}

fn next_actions(checks: &[Check]) -> Vec<String> {
    let mut out = Vec::new();
    for command in checks
        .iter()
        .filter(|check| check.status != Status::Ok)
        .filter_map(|check| check.fix_command.as_deref())
    {
        if !out.iter().any(|existing| existing == command) {
            out.push(command.to_string());
        }
    }
    out
}

fn push_installed_receipts(checks: &mut Vec<Check>, store: &Result<ConfigStore>) {
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            checks.push(check(
                "installed_receipts",
                "installed receipts",
                Status::Fail,
                format!("could not load config: {e}"),
                Some("agentstack config show".to_string()),
            ));
            return;
        }
    };
    let resolver = TargetResolver::new(store);
    let mut targets_seen = 0usize;
    let mut receipts_seen = 0usize;
    for target in doctor_targets() {
        let Ok(resolved) = resolver.resolve(target) else {
            continue;
        };
        if !resolved.path.is_dir() {
            continue;
        }
        targets_seen += 1;
        match count_receipts(&resolved.path) {
            Ok(count) => receipts_seen += count,
            Err(e) => {
                checks.push(check(
                    "installed_receipts",
                    "installed receipts",
                    Status::Fail,
                    format!("{}: {e}", resolved.path.display()),
                    Some("agentstack install list".to_string()),
                ));
                return;
            }
        }
    }
    checks.push(check(
        "installed_receipts",
        "installed receipts",
        Status::Ok,
        format!("{receipts_seen} receipt(s) across {targets_seen} existing target(s)"),
        None,
    ));
}

fn doctor_targets() -> Vec<InstallTarget> {
    InstallTarget::ALL.to_vec()
}

fn count_receipts(root: &Path) -> Result<usize> {
    let mut count = 0usize;
    let entries =
        fs::read_dir(root).with_context(|| format!("failed to read `{}`", root.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to read entry in `{}`", root.display()))?;
        let path = entry.path();
        if path.is_dir() && path.join(RECEIPT_FILE).is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn check_dir_present_or_creatable(code: &str, name: &str, dir: &Path) -> Check {
    if dir.exists() {
        if !dir.is_dir() {
            return check(
                code,
                name,
                Status::Fail,
                format!("{} (exists but is not a directory)", dir.display()),
                None,
            );
        }
        if writable_dir(dir) {
            return check(
                code,
                name,
                Status::Ok,
                format!("{} (writable)", dir.display()),
                None,
            );
        }
        return check(
            code,
            name,
            Status::Fail,
            format!("{} (exists but is not writable)", dir.display()),
            None,
        );
    }
    // Dir doesn't exist yet — that's fine for cache/config. Walk past
    // missing intermediate parents (cache lives under the config dir).
    if parent_creatable(dir) {
        return check(
            code,
            name,
            Status::Ok,
            format!("{} (will be created on first use)", dir.display()),
            None,
        );
    }
    check(
        code,
        name,
        Status::Warn,
        format!(
            "{} (parent does not exist or is not writable)",
            dir.display()
        ),
        None,
    )
}

fn check(
    code: impl Into<String>,
    name: impl Into<String>,
    status: Status,
    detail: impl Into<String>,
    fix_command: Option<String>,
) -> Check {
    Check {
        code: code.into(),
        name: name.into(),
        status,
        detail: detail.into(),
        fix_command,
    }
}
