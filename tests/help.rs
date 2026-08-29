//! Help / discoverability tests.
//!
//! These don't snapshot the full help text (that would churn on every clap
//! upgrade), but they DO assert that the help output mentions every command
//! and global flag the CLI advertises. This is the line of defense against
//! a command silently disappearing or a global flag being lost in a refactor.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn root_help_lists_every_subcommand() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in [
        "skill",
        "stack",
        "team",
        "install",
        "auth",
        "registry",
        "target",
        "audit",
        "config",
        "cache",
        "sync",
        "doctor",
        "completion",
    ] {
        assert!(
            out.contains(&format!("\n  {sub}")),
            "expected `--help` command list to mention `{sub}`; got:\n{out}"
        );
    }
    assert!(
        out.contains("Start:"),
        "expected compact start flow; got:\n{out}"
    );
    assert!(
        out.contains("Publish:"),
        "expected compact publish flow; got:\n{out}"
    );
    assert!(
        out.contains("`acme` is an example org"),
        "expected root help to label acme as an example org; got:\n{out}"
    );
    assert!(
        out.contains("Headless:"),
        "expected compact headless flow; got:\n{out}"
    );
    assert!(
        out.contains("More: agentstack <command> --help; README.md."),
        "expected root help footer to point to focused command help and quickstart; got:\n{out}"
    );
    assert!(
        !out.contains("Command areas:"),
        "expected root help footer not to repeat the command list; got:\n{out}"
    );
}

#[test]
fn root_help_lists_global_flags() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("--json"));
    assert!(out.contains("--no-input"));
    assert!(out.contains("--verbose"));
    assert!(out.contains("--quiet"));
}

#[test]
fn root_help_stays_compact_and_wrapped() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let line_count = out.lines().count();
    assert!(
        line_count <= 70,
        "expected root help to stay compact, got {line_count} lines:\n{out}"
    );
    for line in out.lines() {
        assert!(
            line.len() <= 100,
            "expected root help lines to stay wrapped, got {} chars:\n{line}\n\n{out}",
            line.len()
        );
    }
}

#[test]
fn subcommand_help_inherits_global_flags() {
    // Global flags should show up in every subcommand's --help.
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn each_subcommand_has_help() {
    for sub in [
        "skill",
        "stack",
        "team",
        "install",
        "auth",
        "registry",
        "target",
        "audit",
        "config",
        "cache",
        "sync",
        "doctor",
        "completion",
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .args([sub, "--help"])
            .assert()
            .success();
    }
}

#[test]
fn nested_subcommands_have_help() {
    for args in [
        &["skill", "init", "--help"][..],
        &["skill", "validate", "--help"],
        &["skill", "lint", "--help"],
        &["skill", "inspect", "--help"],
        &["skill", "security-scan", "--help"],
        &["skill", "scan", "--help"],
        &["skill", "pack", "--help"],
        &["skill", "unpack", "--help"],
        &["skill", "list", "--help"],
        &["skill", "search", "--help"],
        &["skill", "candidates", "--help"],
        &["skill", "show", "--help"],
        &["skill", "status", "--help"],
        &["skill", "diff", "--help"],
        &["skill", "push", "--help"],
        &["skill", "export", "--help"],
        &["skill", "install", "--help"],
        &["skill", "update", "--help"],
        &["skill", "uninstall", "--help"],
        &["skill", "visibility", "show", "--help"],
        &["skill", "visibility", "set", "--help"],
        &["skill", "audit", "--help"],
        &["skill", "version", "list", "--help"],
        &["skill", "version", "show", "--help"],
        &["skill", "version", "approve", "--help"],
        &["skill", "version", "yank", "--help"],
        &["skill", "version", "deprecate", "--help"],
        &["stack", "create", "--help"],
        &["stack", "list", "--help"],
        &["stack", "show", "--help"],
        &["stack", "status", "--help"],
        &["stack", "add", "--help"],
        &["stack", "remove", "--help"],
        &["stack", "resolve", "--help"],
        &["stack", "export", "--help"],
        &["stack", "install", "--help"],
        &["stack", "update", "--help"],
        &["stack", "uninstall", "--help"],
        &["stack", "visibility", "show", "--help"],
        &["stack", "visibility", "set", "--help"],
        &["stack", "audit", "--help"],
        &["install", "list", "--help"],
        &["install", "why", "--help"],
        &["install", "update", "--help"],
        &["install", "doctor", "--help"],
        &["install", "unlock", "--help"],
        &["auth", "login", "--help"],
        &["auth", "status", "--help"],
        &["auth", "logout", "--help"],
        &["auth", "whoami", "--help"],
        &["team", "create", "--help"],
        &["team", "list", "--help"],
        &["team", "inspect", "--help"],
        &["team", "add-member", "--help"],
        &["team", "remove-member", "--help"],
        &["team", "set-role", "--help"],
        &["target", "list", "--help"],
        &["target", "detect", "--help"],
        &["target", "setup", "--help"],
        &["target", "path", "--help"],
        &["target", "set", "--help"],
        &["target", "unset", "--help"],
        &["audit", "list", "--help"],
        &["audit", "show", "--help"],
        &["config", "path", "--help"][..],
        &["config", "show", "--help"],
        &["cache", "path", "--help"],
        &["cache", "list", "--help"],
        &["cache", "remove", "--help"],
        &["registry", "ping", "--help"],
        &["registry", "use", "--help"],
        &["registry", "show", "--help"],
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .args(args)
            .assert()
            .success();
    }
}

#[test]
fn core_leaf_help_includes_examples() {
    for args in [
        &["skill", "init", "--help"][..],
        &["skill", "validate", "--help"],
        &["skill", "lint", "--help"],
        &["skill", "inspect", "--help"],
        &["skill", "scan", "--help"],
        &["skill", "pack", "--help"],
        &["completion", "--help"],
    ] {
        let assert = Command::cargo_bin("agentstack")
            .unwrap()
            .args(args)
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(
            out.contains("Examples:"),
            "expected examples in help for {:?}; got:\n{out}",
            args
        );
    }
}

#[test]
fn help_text_states_product_boundaries_and_secret_handling() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("does not execute agents"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["auth", "login", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--token-stdin"))
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN_PATH"))
        .stdout(predicate::str::contains("Machine path"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("agentstack registry ping --auth"))
        .stdout(predicate::str::contains("agentstack doctor"))
        .stdout(predicate::str::contains("AGENTSTACK_TOKEN_PATH"));
}

#[test]
fn help_text_teaches_stack_consumption_and_headless_env_contract() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("skills and stacks"))
        .stdout(predicate::str::contains("stack install"))
        .stdout(predicate::str::contains("AGENTSTACK_REGISTRY_URL"))
        .stdout(predicate::str::contains("--json --no-input"))
        .stdout(predicate::str::contains("agentstack stack export"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("target runtime"))
        .stdout(predicate::str::contains(
            "agentstack stack install acme/engineering-default",
        ));

    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "export", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unmanaged stack"))
        .stdout(predicate::str::contains(
            "agentstack stack export acme/engineering-default",
        ));
}

#[test]
fn install_help_hides_reserved_name_flag() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--name").not());
}

#[test]
fn install_list_help_names_filters() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["install", "list", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Restrict results to one install target",
        ))
        .stdout(predicate::str::contains("Receipt kind to list"))
        .stdout(predicate::str::contains(
            "possible values: skill, stack, all",
        ));
}

#[test]
fn target_set_help_names_builtin_targets() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["target", "set", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for target in [
        "claude-code",
        "codex",
        "repo-claude-code",
        "claude-code-repo",
        "repo-codex",
        "codex-repo",
        "local",
    ] {
        assert!(
            out.contains(target),
            "expected `target set --help` to mention `{target}`; got:\n{out}"
        );
    }
}

#[test]
fn auth_login_help_prefers_safe_token_inputs() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["auth", "login", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("--token-stdin"));
    assert!(out.contains("AGENTSTACK_TOKEN_PATH"));
    assert!(
        !out.contains("Passing --token <TOKEN>"),
        "expected hidden --token flag to stay out of canonical guidance; got:\n{out}"
    );
}

#[test]
fn stack_install_help_names_codex_and_claude_repo_targets() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "install", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("codex-repo"));
    assert!(out.contains("claude-code-repo"));
}

#[test]
fn stack_add_help_explains_version_policy() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "add", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("Version policy: `current`"));
    assert!(out.contains("--version-policy <POLICY>"));
    assert!(out.contains("--pin-version <VERSION>"));
}

#[test]
fn stack_remove_help_documents_dry_run_and_yes() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["stack", "remove", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("--dry-run"));
    assert!(out.contains("--yes"));
    assert!(
        out.contains("agentstack stack remove acme/engineering-default acme/code-review --dry-run")
    );
    assert!(
        out.contains("agentstack stack remove acme/engineering-default acme/code-review --yes")
    );
}

#[test]
fn push_help_does_not_advertise_version_flag() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "push", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !out.contains("--version"),
        "expected `push --help` to omit --version; got:\n{out}"
    );
}

#[test]
fn skill_push_help_advertises_team_scope() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "push", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains("--team <TEAM>"),
        "expected `skill push --help` to advertise team flags; got:\n{out}"
    );
    assert!(
        out.contains("--scope <SCOPE>"),
        "expected `skill push --help` to advertise --scope; got:\n{out}"
    );
}

#[test]
fn high_value_leaf_help_contains_literal_examples() {
    for (args, expected) in [
        (
            &["skill", "update", "--help"][..],
            "agentstack skill update code-review --target codex-repo --check",
        ),
        (
            &["skill", "show", "--help"],
            "agentstack skill show code-review --target codex-repo",
        ),
        (
            &["skill", "uninstall", "--help"],
            "agentstack skill uninstall code-review --target codex-repo --dry-run",
        ),
        (
            &["stack", "update", "--help"],
            "agentstack stack update acme/engineering-default --target codex-repo --check",
        ),
        (
            &["stack", "show", "--help"],
            "agentstack stack show acme/engineering-default --target codex-repo",
        ),
        (
            &["stack", "uninstall", "--help"],
            "agentstack stack uninstall acme/engineering-default --target codex-repo --dry-run",
        ),
        (
            &["skill", "push", "--help"],
            "agentstack skill push ./my-skill --org acme --scope team --team platform",
        ),
        (
            &["skill", "version", "approve", "--help"],
            "agentstack skill version approve acme/code-review@2",
        ),
        (
            &["stack", "visibility", "show", "--help"],
            "Stack ref `stack` or `org/stack`",
        ),
        (
            &["audit", "list", "--help"],
            "agentstack audit list --org acme",
        ),
        (
            &["audit", "show", "--help"],
            "agentstack audit show aud_123 --org acme",
        ),
        (
            &["team", "add-member", "--help"],
            "agentstack team add-member acme/platform user@example.com --role member",
        ),
        (
            &["team", "set-role", "--help"],
            "agentstack team set-role acme/platform admin@example.com --role team_admin",
        ),
        (
            &["skill", "status", "--help"],
            "agentstack skill status acme/code-review",
        ),
        (
            &["stack", "audit", "--help"],
            "agentstack stack audit acme/engineering-default",
        ),
    ] {
        let assert = Command::cargo_bin("agentstack")
            .unwrap()
            .args(args)
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(
            out.contains(expected),
            "expected `agentstack {}` help to contain `{expected}`; got:\n{out}",
            args.join(" ")
        );
    }
}

#[test]
fn dead_compatibility_grammar_is_rejected() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["audit", "--help"])
        .assert()
        .success();
    let audit_help = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !audit_help.contains("\n  skill"),
        "expected `audit skill` compatibility form to stay out of primary help; got:\n{audit_help}"
    );
    assert!(
        !audit_help.contains("\n  stack"),
        "expected `audit stack` compatibility form to stay out of primary help; got:\n{audit_help}"
    );

    for args in [
        &["audit", "skill", "--help"][..],
        &["audit", "stack", "--help"],
        &["init", "--help"],
        &["installed", "list", "--help"],
        &["targets", "list", "--help"],
        &["install", "show", "--help"],
        &["install", "status", "--help"],
        &["install", "remove", "--help"],
        &["skill", "installed", "--help"],
        &["stack", "installed", "--help"],
        &["config", "set-target", "--help"],
        &["config", "unset-target", "--help"],
        &["registry", "set", "--help"],
        &["registry", "get", "--help"],
        &["completions", "bash"],
    ] {
        Command::cargo_bin("agentstack")
            .unwrap()
            .args(args)
            .assert()
            .failure();
    }

    let export = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "export", "--help"])
        .assert()
        .success();
    let export_help = String::from_utf8(export.get_output().stdout.clone()).unwrap();
    assert!(
        !export_help.contains("--version"),
        "expected `skill export --version` to stay out of primary help; got:\n{export_help}"
    );

    for args in [
        &["team", "add-member", "--help"][..],
        &["team", "set-role", "--help"],
    ] {
        let assert = Command::cargo_bin("agentstack")
            .unwrap()
            .args(args)
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(out.contains("member or team_admin"));
        assert!(
            !out.contains("lead"),
            "expected team role help to omit legacy `lead`; got:\n{out}"
        );
    }
}

#[test]
fn push_rejects_version_flag() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "push", "--org", "acme", "--version", "1.2.3"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--version"));
}

#[test]
fn target_setup_help_describes_noninteractive_options() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["target", "setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Register this absolute directory for the target without prompting",
        ))
        .stdout(predicate::str::contains(
            "Accept the platform default path for the target without prompting",
        ));
}

#[test]
fn skill_install_help_names_local_and_registry_sources() {
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "install", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("Local skill directory") && stdout.contains("org/skill"),
        "expected skill install help to mention local-vs-remote source forms; got:\n{stdout}"
    );
}

#[test]
fn yank_help_explains_default_refusal_clearly() {
    // The previous wording ("Refused on default fresh install") was parsed
    // by humans as nonsense. The new copy must name the concrete behavior.
    let assert = Command::cargo_bin("agentstack")
        .unwrap()
        .args(["skill", "version", "yank", "--help"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !out.contains("Refused on default fresh install"),
        "expected `skill version yank --help` to drop confusing wording; got:\n{out}"
    );
}

#[test]
fn version_flag_works() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("agentstack"));
}
