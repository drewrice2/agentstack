use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::ConfigStore;
use crate::error::CliError;
use crate::output::Ctx;
use crate::targets::{InstallTarget, TargetDetection, TargetResolver, default_target_path};

pub struct Args {
    pub target: Option<String>,
    pub path: Option<PathBuf>,
    pub yes: bool,
}

pub fn run(ctx: &Ctx, args: Args) -> Result<()> {
    let mut store = ConfigStore::load().context("failed to load config")?;

    if let Some(path) = args.path {
        let target = parse_target(args.target.as_deref())?;
        return configure_target(ctx, &mut store, target, path);
    }

    let Some(target_name) = args.target.as_deref() else {
        return run_autodetect(ctx, &mut store, args.yes);
    };

    let target = parse_target(Some(target_name))?;
    run_target(ctx, &mut store, target, args.yes)
}

fn run_target(ctx: &Ctx, store: &mut ConfigStore, target: InstallTarget, yes: bool) -> Result<()> {
    if yes {
        if let Some(existing) = store
            .target_override(target.as_str())
            .map(Path::to_path_buf)
        {
            return already_configured(ctx, store, target, &existing);
        }
        let path = default_target_path(target).with_context(|| {
            format!(
                "could not determine a default path for `{}`; rerun with `--path <absolute-path>`",
                target.as_str()
            )
        })?;
        return configure_target(ctx, store, target, path);
    }

    if ctx.json {
        print_json(ctx, store, None, None, Vec::new())?;
        return Ok(());
    }

    print_status(ctx, store);

    if all_targets_configured(store) {
        ctx.say("");
        ctx.say("all known install targets are configured");
        ctx.say("next: agentstack target list");
        return Ok(());
    }

    if let Some(existing) = store
        .target_override(target.as_str())
        .map(Path::to_path_buf)
    {
        ctx.say("");
        return already_configured(ctx, store, target, &existing);
    }

    let Some(default_path) = default_target_path(target) else {
        ctx.say("");
        ctx.say(format!(
            "no default path could be detected for `{}`",
            target.as_str()
        ));
        print_next_commands(ctx, target, None);
        return Ok(());
    };

    if ctx.can_prompt() {
        let accepted = ctx.prompt_confirm(
            format!(
                "Configure `{}` at {}?",
                target.as_str(),
                default_path.display()
            ),
            "setup cannot prompt in this context; rerun with `--yes` or `--path <absolute-path>`",
        )?;
        if accepted {
            return configure_target(ctx, store, target, default_path);
        }
        ctx.say("no changes made");
        print_next_commands(ctx, target, Some(&default_path));
        return Ok(());
    }

    ctx.say("");
    ctx.say("no changes made because setup is running non-interactively");
    Err(noninteractive_target_setup_error(target, Some(&default_path)).into())
}

fn noninteractive_target_setup_error(
    target: InstallTarget,
    default_path: Option<&Path>,
) -> CliError {
    let mut error = CliError::new(
        "target_setup_requires_confirmation",
        format!(
            "target setup for `{}` requires `--yes` or `--path <absolute-path>` in non-interactive mode",
            target.as_str()
        ),
    )
    .resource(target.as_str())
    .action("target_setup");
    if default_path.is_some() {
        error = error.next_command(format!("agentstack target setup {} --yes", target.as_str()));
    } else {
        error = error.next_command(format!(
            "agentstack target setup {} --path <absolute-path>",
            target.as_str()
        ));
    }
    error
}

fn run_autodetect(ctx: &Ctx, store: &mut ConfigStore, yes: bool) -> Result<()> {
    let detections = TargetResolver::new(store).detect_all();
    let groups = classify_detections(detections);

    if ctx.json {
        let registered = if yes {
            register_detectable_targets(store, &groups.detectable)?
        } else {
            Vec::new()
        };
        print_autodetect_json(ctx, store, &groups, registered)?;
        return Ok(());
    }

    print_autodetect_status(ctx, store, &groups.all);

    if all_targets_configured(store) {
        ctx.say("");
        ctx.say("all targets configured");
        ctx.say("next: agentstack target list");
        return Ok(());
    }

    if yes {
        let registered = register_detectable_targets(store, &groups.detectable)?;
        print_autodetect_result(ctx, &groups, &registered);
        return Ok(());
    }

    if !groups.detectable.is_empty() && ctx.can_prompt() {
        ctx.say("");
        let target_names = detection_target_names(&groups.detectable).join(", ");
        let accepted = prompt_register_targets(ctx, groups.detectable.len(), &target_names)?;
        if accepted {
            let registered = register_detectable_targets(store, &groups.detectable)?;
            print_autodetect_result(ctx, &groups, &registered);
            return Ok(());
        }
        ctx.say("no changes made");
        print_autodetect_next_commands(ctx, &groups);
        return Ok(());
    }

    ctx.say("");
    ctx.say("no changes made because setup is running non-interactively");
    print_autodetect_next_commands(ctx, &groups);
    Ok(())
}

fn already_configured(
    ctx: &Ctx,
    store: &ConfigStore,
    target: InstallTarget,
    path: &Path,
) -> Result<()> {
    if ctx.json {
        print_json(ctx, store, None, None, Vec::new())?;
        return Ok(());
    }
    ctx.say(format!(
        "target `{}` is already configured -> {}",
        target.as_str(),
        path.display()
    ));
    ctx.say("next: agentstack target detect");
    Ok(())
}

fn parse_target(target: Option<&str>) -> Result<InstallTarget> {
    match target {
        Some(target) => InstallTarget::parse(target),
        None => Ok(InstallTarget::Local),
    }
}

/// Persist `target -> path` as an override in `store` and ensure the
/// destination directory exists. Shared by interactive `target setup` and the
/// implicit auto-register path used by `skill install --target <name>` when
/// no override has been recorded yet.
pub(crate) fn auto_register(
    store: &mut ConfigStore,
    target: InstallTarget,
    path: &Path,
) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "setup target path must be absolute (got `{}`)",
            path.display()
        );
    }
    fs::create_dir_all(path).with_context(|| format!("failed to create `{}`", path.display()))?;
    store.set_target(target.as_str().to_string(), path.to_path_buf());
    store.save().context("failed to write config")?;
    Ok(())
}

fn configure_target(
    ctx: &Ctx,
    store: &mut ConfigStore,
    target: InstallTarget,
    path: PathBuf,
) -> Result<()> {
    auto_register(store, target, &path)?;

    if ctx.json {
        print_json(
            ctx,
            store,
            Some(target),
            Some(&path),
            vec![registered_target(target, &path)],
        )?;
        return Ok(());
    }

    ctx.say(format!(
        "configured target `{}` -> {}",
        target.as_str(),
        path.display()
    ));
    ctx.say(format!("config: {}", store.path().display()));
    ctx.say("next:");
    ctx.say(format!(
        "  agentstack skill install <skill> --target {}",
        target.as_str()
    ));
    Ok(())
}

fn print_status(ctx: &Ctx, store: &ConfigStore) {
    ctx.say("agentstack setup");
    ctx.say(format!("config: {}", store.path().display()));
    ctx.say("");
    for row in target_rows(store) {
        let state = if row.configured {
            "configured"
        } else {
            "not configured"
        };
        let usability = if row.usable { "usable" } else { "needs setup" };
        match row.path {
            Some(path) => ctx.say(format!(
                "{}: {state} -> {path} ({}, {usability})",
                row.target, row.source
            )),
            None => ctx.say(format!("{}: {state} ({usability})", row.target)),
        }
    }
}

fn print_autodetect_status(ctx: &Ctx, store: &ConfigStore, detections: &[TargetDetection]) {
    ctx.say("agentstack setup");
    ctx.say(format!("config: {}", store.path().display()));
    ctx.say("");
    ctx.say("detected install targets:");

    let width = detections
        .iter()
        .map(|row| row.target.len())
        .max()
        .unwrap_or(0);

    for row in detections {
        let state = if row.configured {
            "configured"
        } else {
            "not configured"
        };
        let usability = if row.usable { "usable" } else { "needs setup" };
        let mut details = format!("{}, {usability}", row.source);
        if !row.usable
            && let Some(fix_command) = &row.fix_command
        {
            details.push_str("; fix: ");
            details.push_str(fix_command);
        }
        match &row.path {
            Some(path) => ctx.say(format!(
                "  {:width$}: {state} -> {} ({details})",
                row.target,
                path.display()
            )),
            None => ctx.say(format!("  {:width$}: {state} ({details})", row.target)),
        }
    }
}

fn print_next_commands(ctx: &Ctx, target: InstallTarget, path: Option<&Path>) {
    ctx.say("next:");
    match path {
        Some(path) => {
            ctx.say(format!(
                "  agentstack target setup {} --path {}",
                target.as_str(),
                shell_path(path)
            ));
            ctx.say(format!(
                "  agentstack target set {} --path {}",
                target.as_str(),
                shell_path(path)
            ));
        }
        None => {
            ctx.say(format!(
                "  agentstack target setup {} --path <absolute-path>",
                target.as_str()
            ));
            ctx.say(format!(
                "  agentstack target set {} --path <absolute-path>",
                target.as_str()
            ));
        }
    }
}

fn all_targets_configured(store: &ConfigStore) -> bool {
    InstallTarget::ALL
        .iter()
        .all(|target| store.target_override(target.as_str()).is_some())
}

fn target_rows(store: &ConfigStore) -> Vec<SetupTarget> {
    let resolver = TargetResolver::new(store);
    target_rows_from_detections(&resolver.detect_all())
}

fn target_rows_from_detections(detections: &[TargetDetection]) -> Vec<SetupTarget> {
    detections
        .iter()
        .map(|row| {
            let path = row.path.as_ref().map(|path| path.display().to_string());
            SetupTarget {
                target: row.target,
                configured: row.configured,
                path,
                source: row.source,
                exists: row.exists,
                writable: row.writable,
                usable: row.usable,
                fix_command: row.fix_command.clone(),
            }
        })
        .collect()
}

fn print_json(
    ctx: &Ctx,
    store: &ConfigStore,
    configured_target: Option<InstallTarget>,
    configured_path: Option<&Path>,
    registered: Vec<RegisteredTarget>,
) -> Result<()> {
    let target = configured_target.unwrap_or(InstallTarget::Local);
    let default_path = configured_path
        .map(Path::to_path_buf)
        .or_else(|| default_target_path(target));
    let next_guidance = next_guidance(target, default_path.as_deref(), configured_target.is_some());
    let payload = SetupJson {
        config: store.path().display().to_string(),
        configured_now: configured_target.is_some(),
        target: configured_target.map(|target| target.as_str().to_string()),
        path: configured_path.map(|path| path.display().to_string()),
        targets: target_rows(store),
        registered,
        no_input: ctx.no_input(),
        next_commands: next_guidance.commands,
        next_command_templates: next_guidance.templates,
    };
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn print_autodetect_json(
    ctx: &Ctx,
    store: &ConfigStore,
    groups: &DetectionGroups,
    registered: Vec<RegisteredTarget>,
) -> Result<()> {
    let configured_now = !registered.is_empty();
    let next_guidance = autodetect_next_guidance(store, groups, configured_now);
    let payload = SetupJson {
        config: store.path().display().to_string(),
        configured_now,
        target: None,
        path: None,
        targets: target_rows(store),
        registered,
        no_input: ctx.no_input(),
        next_commands: next_guidance.commands,
        next_command_templates: next_guidance.templates,
    };
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn next_guidance(target: InstallTarget, path: Option<&Path>, configured_now: bool) -> NextGuidance {
    if configured_now {
        return NextGuidance {
            commands: Vec::new(),
            templates: vec![format!(
                "agentstack skill install <skill> --target {}",
                target.as_str()
            )],
        };
    }

    let commands = match path {
        Some(path) => vec![
            format!(
                "agentstack target setup {} --path {}",
                target.as_str(),
                shell_path(path)
            ),
            format!(
                "agentstack target set {} --path {}",
                target.as_str(),
                shell_path(path)
            ),
        ],
        None => Vec::new(),
    };
    let templates = if path.is_some() {
        Vec::new()
    } else {
        vec![
            format!(
                "agentstack target setup {} --path <absolute-path>",
                target.as_str()
            ),
            format!(
                "agentstack target set {} --path <absolute-path>",
                target.as_str()
            ),
        ]
    };
    NextGuidance {
        commands,
        templates,
    }
}

fn shell_path(path: &Path) -> String {
    let text = path.display().to_string();
    if text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return text;
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[derive(Debug)]
struct DetectionGroups {
    all: Vec<TargetDetection>,
    detectable: Vec<TargetDetection>,
    skipped: Vec<TargetDetection>,
}

fn classify_detections(detections: Vec<TargetDetection>) -> DetectionGroups {
    let mut detectable = Vec::new();
    let mut skipped = Vec::new();

    for detection in &detections {
        if detection.configured {
            continue;
        }
        if detection.usable
            && (detection.exists || immediate_parent_exists(detection.path.as_deref()))
        {
            detectable.push(detection.clone());
        } else {
            skipped.push(detection.clone());
        }
    }

    DetectionGroups {
        all: detections,
        detectable,
        skipped,
    }
}

fn immediate_parent_exists(path: Option<&Path>) -> bool {
    path.and_then(Path::parent)
        .is_some_and(|parent| parent.is_dir())
}

fn register_detectable_targets(
    store: &mut ConfigStore,
    detections: &[TargetDetection],
) -> Result<Vec<RegisteredTarget>> {
    let mut registered = Vec::new();

    for detection in detections {
        let target = InstallTarget::parse(detection.target)?;
        let path = default_target_path(target).with_context(|| {
            format!(
                "could not determine a default path for `{}`; rerun with `--target {} --path <absolute-path>`",
                target.as_str(),
                target.as_str()
            )
        })?;
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create `{}`", path.display()))?;
        store.set_target(target.as_str().to_string(), path.clone());
        registered.push(registered_target(target, &path));
    }

    if !registered.is_empty() {
        store.save().context("failed to write config")?;
    }

    Ok(registered)
}

fn registered_target(target: InstallTarget, path: &Path) -> RegisteredTarget {
    RegisteredTarget {
        target: target.as_str().to_string(),
        path: path.display().to_string(),
    }
}

fn print_autodetect_result(ctx: &Ctx, groups: &DetectionGroups, registered: &[RegisteredTarget]) {
    ctx.say("");
    if registered.is_empty() {
        ctx.say("no usable unconfigured targets detected");
        print_autodetect_next_commands(ctx, groups);
        return;
    }

    for row in registered {
        ctx.say(format!(
            "configured target `{}` -> {}",
            row.target, row.path
        ));
    }
    ctx.say("next:");
    ctx.say("  agentstack skill install <skill> --target <target>");
}

fn print_autodetect_next_commands(ctx: &Ctx, groups: &DetectionGroups) {
    let commands = autodetect_hint_commands(groups, |row| {
        Some(format!("agentstack target setup {}", row.target))
    });
    if commands.is_empty() {
        return;
    }

    ctx.say("next:");
    for command in commands {
        ctx.say(format!("  {command}"));
    }
}

fn autodetect_next_guidance(
    store: &ConfigStore,
    groups: &DetectionGroups,
    configured_now: bool,
) -> NextGuidance {
    if configured_now || all_targets_configured(store) {
        return NextGuidance {
            commands: Vec::new(),
            templates: vec!["agentstack skill install <skill> --target <target>".to_string()],
        };
    }
    let mut commands = Vec::new();
    let mut templates = Vec::new();
    for command in autodetect_hint_commands(groups, |row| row.fix_command.clone()) {
        push_guidance(command, &mut commands, &mut templates);
    }
    NextGuidance {
        commands,
        templates,
    }
}

/// Shared autodetect hint list; `detectable_hint` picks the per-row command
/// suggested for not-yet-configured detectable targets (the human and JSON
/// renderings differ there).
fn autodetect_hint_commands(
    groups: &DetectionGroups,
    detectable_hint: impl Fn(&TargetDetection) -> Option<String>,
) -> Vec<String> {
    let mut commands = Vec::new();
    if !groups.detectable.is_empty() {
        commands.push("agentstack target setup --yes".to_string());
        commands.extend(groups.detectable.iter().filter_map(detectable_hint));
    }
    commands.extend(
        groups
            .skipped
            .iter()
            .filter_map(|row| row.fix_command.clone()),
    );
    commands
}

fn push_guidance(command: String, commands: &mut Vec<String>, templates: &mut Vec<String>) {
    let out = if command.contains('<') {
        templates
    } else {
        commands
    };
    if !out.iter().any(|existing| existing == &command) {
        out.push(command);
    }
}

fn detection_target_names(detections: &[TargetDetection]) -> Vec<&'static str> {
    detections.iter().map(|row| row.target).collect()
}

fn prompt_register_targets(ctx: &Ctx, count: usize, target_names: &str) -> Result<bool> {
    if !ctx.can_prompt() {
        bail!("setup cannot prompt in this context; rerun with `--yes`");
    }

    eprint!("Register {count} target(s): {target_names}? [Y/n] ");
    if io::stderr().flush().is_err() {
        return Ok(false);
    }
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer.is_empty() || matches!(answer.as_str(), "y" | "yes"))
}

#[derive(Serialize)]
struct SetupJson {
    config: String,
    configured_now: bool,
    target: Option<String>,
    path: Option<String>,
    targets: Vec<SetupTarget>,
    registered: Vec<RegisteredTarget>,
    no_input: bool,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    next_commands: Vec<String>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    next_command_templates: Vec<String>,
}

struct NextGuidance {
    commands: Vec<String>,
    templates: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RegisteredTarget {
    target: String,
    path: String,
}

#[derive(Serialize)]
struct SetupTarget {
    target: &'static str,
    configured: bool,
    path: Option<String>,
    source: &'static str,
    exists: bool,
    writable: bool,
    usable: bool,
    fix_command: Option<String>,
}
