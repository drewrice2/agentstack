use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn config_path_prints_a_directory_path() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agentstack"));
}
