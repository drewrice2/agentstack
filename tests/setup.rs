use assert_cmd::Command;
use assert_fs::TempDir;
use assert_fs::fixture::ChildPath;
use assert_fs::prelude::*;
use predicates::prelude::*;
use serde_json::Value;

fn cmd(cfg: &std::path::Path) -> Command {
    let mut command = Command::cargo_bin("agentstack").unwrap();
    command.env("AGENTSTACK_CONFIG_DIR", cfg);
    command
}

#[test]
fn autodetect_no_input_prints_hint_without_writing_config() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    cmd_with_home(cfg.path(), home.path())
        .args(["--no-input", "target", "setup"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no changes made because setup is running non-interactively",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("agentstack target setup --yes"))
        .stdout(predicate::str::contains("agentstack target setup local"));

    assert!(!cfg.child("config.toml").path().exists());
}

#[test]
fn setup_without_no_input_uses_noninteractive_fallback_when_stdio_is_captured() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    cmd_with_home(cfg.path(), home.path())
        .args(["target", "setup"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no changes made because setup is running non-interactively",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("agentstack target setup --yes"))
        .stdout(predicate::str::contains("agentstack target setup local"));

    assert!(!cfg.child("config.toml").path().exists());
}

#[test]
fn explicit_target_setup_noninteractive_without_yes_fails() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    cmd_with_home(cfg.path(), home.path())
        .args(["--no-input", "target", "setup", "codex"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "no changes made because setup is running non-interactively",
        ))
        .stderr(predicate::str::contains(
            "requires `--yes` or `--path <absolute-path>`",
        ))
        .stderr(predicate::str::contains(
            "next: agentstack target setup codex --yes",
        ));

    assert!(!cfg.child("config.toml").path().exists());
}

#[test]
fn setup_json_is_parseable_and_never_prompts() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    let output = cmd_with_home(cfg.path(), home.path())
        .args(["--json", "target", "setup"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["configured_now"], Value::Bool(false));
    assert_eq!(json["no_input"], Value::Bool(true));
    assert!(json["targets"].as_array().unwrap().len() >= 3);
    assert!(json["registered"].as_array().unwrap().is_empty());
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command.as_str() == Some("agentstack target setup --yes"))
    );
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );
    assert!(!cfg.child("config.toml").path().exists());
}

#[test]
fn autodetect_json_registered_field_lists_added_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    let output = cmd_with_home(cfg.path(), home.path())
        .args(["--json", "target", "setup", "--yes"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["configured_now"], Value::Bool(true));
    let registered = json["registered"].as_array().unwrap();
    assert_eq!(registered.len(), 3);
    for expected in ["claude-code", "codex", "local"] {
        assert!(registered.iter().any(|row| {
            row["target"].as_str() == Some(expected)
                && row["path"]
                    .as_str()
                    .unwrap()
                    .contains(default_target_suffix(expected))
        }));
    }
    assert!(json.get("next_commands").is_none());
    assert!(
        json["next_command_templates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| {
                command.as_str() == Some("agentstack skill install <skill> --target <target>")
            })
    );
}

#[test]
fn autodetect_registers_all_usable_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    create_default_target_parents(&home);

    cmd_with_home(cfg.path(), home.path())
        .args(["target", "setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains("configured target `claude-code`"))
        .stdout(predicate::str::contains("configured target `codex`"))
        .stdout(predicate::str::contains("configured target `local`"))
        .stdout(predicate::str::contains(
            "agentstack skill install <skill> --target <target>",
        ));

    for target in ["claude-code", "codex", "local"] {
        let path = default_target_path_for_home(&home, target);
        assert!(path.path().is_dir());
        assert!(
            std::fs::read_to_string(cfg.child("config.toml").path())
                .unwrap()
                .contains(path.path().to_str().unwrap())
        );
    }
}

#[test]
fn autodetect_skips_already_configured_targets() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let home = tmp.child("home");
    home.create_dir_all().unwrap();
    let claude = tmp.child("configured-claude");
    let codex = tmp.child("configured-codex");
    claude.create_dir_all().unwrap();
    codex.create_dir_all().unwrap();

    cmd_with_home(cfg.path(), home.path())
        .args([
            "target",
            "set",
            "claude-code",
            "--path",
            claude.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    cmd_with_home(cfg.path(), home.path())
        .args([
            "target",
            "set",
            "codex",
            "--path",
            codex.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let before = std::fs::read_to_string(cfg.child("config.toml").path()).unwrap();
    cmd_with_home(cfg.path(), home.path())
        .args(["target", "setup", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "no usable unconfigured targets detected",
        ))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack target setup local --path",
        ));
    let after = std::fs::read_to_string(cfg.child("config.toml").path()).unwrap();

    assert_eq!(after, before);
}

#[test]
fn setup_target_json_registered_field_lists_added_target() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target = tmp.child("skills");

    let output = cmd(cfg.path())
        .args([
            "--json",
            "target",
            "setup",
            "local",
            "--path",
            target.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).unwrap();
    let registered = json["registered"].as_array().unwrap();
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0]["target"].as_str(), Some("local"));
    assert_eq!(
        registered[0]["path"].as_str(),
        Some(target.path().to_str().unwrap())
    );
}

#[test]
fn setup_json_next_commands_still_support_single_target() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    let output = cmd(cfg.path())
        .args(["--json", "target", "setup", "local"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert!(
        json["next_commands"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .contains("agentstack target setup local --path")
    );
    assert!(
        json["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .all(|command| !command.as_str().unwrap().contains('<'))
    );
}

#[test]
fn setup_path_defaults_to_local_and_writes_config() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let target = tmp.child("skills");

    cmd(cfg.path())
        .args(["target", "setup", "--path", target.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("configured target `local`"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(
            "agentstack skill install <skill> --target local",
        ));

    assert!(target.path().is_dir());
    cmd(cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("[targets]"))
        .stdout(predicate::str::contains("local"))
        .stdout(predicate::str::contains(target.path().to_str().unwrap()));
}

#[test]
fn setup_target_yes_does_not_overwrite_existing_target() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();
    let original = tmp.child("original-skills");
    original.create_dir_all().unwrap();

    cmd(cfg.path())
        .args([
            "target",
            "set",
            "local",
            "--path",
            original.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    cmd(cfg.path())
        .args(["target", "setup", "local", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("already configured"))
        .stdout(predicate::str::contains("next:"))
        .stdout(predicate::str::contains(original.path().to_str().unwrap()));

    cmd(cfg.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains(original.path().to_str().unwrap()));
}

#[test]
fn setup_rejects_relative_target_paths() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.child("cfg");
    cfg.create_dir_all().unwrap();

    cmd(cfg.path())
        .args(["target", "setup", "--path", "relative/skills"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be absolute"))
        .stderr(predicate::str::contains("relative/skills"));
}

fn cmd_with_home(cfg: &std::path::Path, home: &std::path::Path) -> Command {
    let mut command = cmd(cfg);
    let cwd = cfg
        .parent()
        .expect("test config dir should have a parent")
        .join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    command.current_dir(cwd);
    command.env("HOME", home);
    command
}

fn create_default_target_parents(home: &ChildPath) {
    home.child(".claude").create_dir_all().unwrap();
    home.child(".codex").create_dir_all().unwrap();
    home.child(".agentstack").create_dir_all().unwrap();
}

fn default_target_path_for_home(home: &ChildPath, target: &str) -> ChildPath {
    match target {
        "claude-code" => home.child(".claude").child("skills"),
        "codex" => home.child(".codex").child("skills"),
        "local" => home.child(".agentstack").child("skills"),
        _ => panic!("unexpected target {target}"),
    }
}

fn default_target_suffix(target: &str) -> &'static str {
    match target {
        "claude-code" => ".claude/skills",
        "codex" => ".codex/skills",
        "local" => ".agentstack/skills",
        _ => panic!("unexpected target {target}"),
    }
}
