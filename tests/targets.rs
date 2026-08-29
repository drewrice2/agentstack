use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

fn parse_json(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("expected parseable JSON")
}

#[test]
fn targets_list_shows_known_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("repo-claude-code"))
        .stdout(predicate::str::contains("Claude Code repo skills"))
        .stdout(predicate::str::contains("repo-codex"))
        .stdout(predicate::str::contains("Codex repo skills"))
        .stdout(predicate::str::contains("local"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack target set <target> --path <path>",
        ));
}

#[test]
fn targets_list_quiet_hides_override_hint_but_keeps_table() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["--quiet", "target", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-code"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("repo-claude-code"))
        .stdout(predicate::str::contains("repo-codex"))
        .stdout(predicate::str::contains("local"))
        .stdout(predicate::str::contains("next:").not());
}

#[test]
fn targets_detect_reports_configured_existing_writable_target() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    let target_dir = tmp.child("local-skills");
    target_dir.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            target_dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    let targets = json["targets"].as_array().expect("targets array");
    let local = targets
        .iter()
        .find(|row| row["target"].as_str() == Some("local"))
        .expect("local target row");
    assert_eq!(local["configured"], Value::Bool(true));
    assert_eq!(local["source"].as_str(), Some("override"));
    assert_eq!(
        local["path"].as_str(),
        Some(target_dir.path().to_str().unwrap())
    );
    assert_eq!(local["exists"], Value::Bool(true));
    assert_eq!(local["is_dir"], Value::Bool(true));
    assert_eq!(local["writable"], Value::Bool(true));
    assert_eq!(local["usable"], Value::Bool(true));
    assert_eq!(local["fix_command"], Value::Null);
}

#[test]
fn targets_detect_treats_recursively_creatable_override_as_usable() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    let target_dir = tmp.path().join("missing-parent").join("skills");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            target_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    let local = json["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"].as_str() == Some("local"))
        .expect("local target row");
    assert_eq!(local["configured"], Value::Bool(true));
    assert_eq!(local["exists"], Value::Bool(false));
    assert_eq!(local["creatable"], Value::Bool(true));
    assert_eq!(local["usable"], Value::Bool(true));
    assert_eq!(local["fix_command"], Value::Null);
}

#[test]
fn targets_detect_existing_unconfigured_default_uses_setup_fix_command() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    let local_default = home.child(".agentstack").child("skills");
    local_default.create_dir_all().unwrap();

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    let local = json["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"].as_str() == Some("local"))
        .expect("local target row");
    assert_eq!(local["configured"], Value::Bool(false));
    assert_eq!(local["exists"], Value::Bool(true));
    assert_eq!(local["writable"], Value::Bool(true));
    assert_eq!(local["usable"], Value::Bool(true));
    let expected = format!(
        "agentstack target setup local --path {} --yes",
        local_default.path().display()
    );
    assert_eq!(local["fix_command"].as_str(), Some(expected.as_str()));
}

#[test]
fn targets_detect_reports_unconfigured_targets_with_fix_commands() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    let targets = json["targets"].as_array().expect("targets array");
    assert_eq!(targets.len(), 5);
    let next_commands = json["next_commands"]
        .as_array()
        .expect("next_commands array");
    assert_eq!(next_commands.len(), 5);
    assert!(json.get("next_command_templates").is_none());
    for expected in [
        "claude-code",
        "codex",
        "repo-claude-code",
        "repo-codex",
        "local",
    ] {
        let row = targets
            .iter()
            .find(|row| row["target"].as_str() == Some(expected))
            .expect("target row");
        assert_eq!(row["configured"], Value::Bool(false));
        assert!(row["source"].is_string());
        assert!(
            row["fix_command"]
                .as_str()
                .unwrap()
                .starts_with(&format!("agentstack target setup {expected} --path "))
        );
        assert!(
            next_commands
                .iter()
                .any(|command| command.as_str().is_some_and(|command| command
                    .starts_with(&format!("agentstack target setup {expected} --path "))))
        );
        assert!(next_commands.iter().all(|command| {
            command
                .as_str()
                .is_some_and(|command| !command.contains('<') && !command.contains('>'))
        }));
    }

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["target", "detect"])
        .assert()
        .success()
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack target setup local --path",
        ));
}

#[test]
fn targets_detect_json_omits_next_commands_when_all_targets_are_ready() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();

    for target in [
        "claude-code",
        "codex",
        "repo-claude-code",
        "repo-codex",
        "local",
    ] {
        let path = tmp.child(format!("{target}-skills"));
        path.create_dir_all().unwrap();
        Command::cargo_bin("agentstack")
            .unwrap()
            .env("AGENTSTACK_CONFIG_DIR", cfg.path())
            .env("HOME", home.path())
            .args([
                "target",
                "set",
                target,
                "--path",
                path.path().to_str().unwrap(),
            ])
            .assert()
            .success();
    }

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    assert!(json.get("next_commands").is_none());
    assert!(json.get("next_command_templates").is_none());
    for row in json["targets"].as_array().expect("targets array") {
        assert_eq!(row["usable"], Value::Bool(true));
        assert_eq!(row["fix_command"], Value::Null);
    }
}

#[test]
fn targets_detect_json_splits_placeholder_fix_commands_into_templates() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    let bad_target_path = tmp.child("not-a-directory");
    bad_target_path.write_str("not a directory").unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            bad_target_path.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let out = Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .env("HOME", home.path())
        .args(["--json", "target", "detect"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json(&out);
    let templates = json["next_command_templates"]
        .as_array()
        .expect("next_command_templates array");
    assert!(templates.iter().any(|command| {
        command
            .as_str()
            .is_some_and(|command| command == "agentstack target set local --path <absolute-path>")
    }));
    assert!(
        json["next_commands"]
            .as_array()
            .map(|commands| commands.iter().all(|command| command
                .as_str()
                .is_some_and(|command| !command.contains('<') && !command.contains('>'))))
            .unwrap_or(true)
    );
}

#[test]
fn targets_path_returns_override_when_set() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let custom = tmp.child("custom-claude");

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args([
            "target",
            "set",
            "claude-code",
            "--path",
            custom.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "path", "claude-code"])
        .assert()
        .success()
        .stdout(predicate::str::contains(custom.path().to_str().unwrap()));
}

#[test]
fn targets_path_rejects_unknown() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "path", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown install target"));
}

#[test]
fn config_show_reflects_set_and_unset() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(empty"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack target set <target> --path <path>",
        ))
        .stdout(predicate::str::contains("agentstack registry use <URL>"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "set", "codex", "--path", "/tmp/codex-skills"])
        .assert()
        .success();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[targets]"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("/tmp/codex-skills"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "unset", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed target `codex`"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(empty"))
        .stdout(predicate::str::contains("next:"));
}

#[test]
fn config_show_quiet_hides_empty_helper_but_keeps_config_path() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["--quiet", "config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("config:"))
        .stdout(predicate::str::contains("(empty").not())
        .stdout(predicate::str::contains("next:").not());
}

#[test]
fn config_set_target_rejects_relative_path() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "set", "codex", "--path", "relative/codex-skills"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be absolute"))
        .stderr(predicate::str::contains("relative/codex-skills"));

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(empty"));
}

#[test]
fn hand_edited_relative_target_config_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    cfg.child("config.toml")
        .write_str("[targets]\ncodex = \"relative/codex-skills\"\n")
        .unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to load config"))
        .stderr(predicate::str::contains("must be absolute"))
        .stderr(predicate::str::contains("relative/codex-skills"));
}

#[test]
fn config_unset_target_when_missing_is_noop() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    Command::cargo_bin("agentstack")
        .unwrap()
        .env("AGENTSTACK_CONFIG_DIR", cfg.path())
        .args(["target", "unset", "claude-code"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to do"));
}
