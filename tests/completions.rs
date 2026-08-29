//! Smoke tests for `agentstack completion`. The output is large and shell-
//! specific, so we just assert the command runs cleanly and emits non-empty
//! output that mentions the binary name.

use assert_cmd::Command;
use assert_cmd::cargo::CommandCargoExt;
use predicates::prelude::*;
use std::io::{BufRead, BufReader};
use std::process::{Command as StdCommand, Stdio};

fn run_for(shell: &str) {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["completion", shell])
        .assert()
        .success()
        .stdout(predicate::str::contains("agentstack"));
}

#[test]
fn completions_zsh_treats_broken_pipe_as_success() {
    let mut child = StdCommand::cargo_bin("agentstack")
        .unwrap()
        .args(["completion", "zsh"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader.read_line(&mut first_line).unwrap();
    drop(reader);

    assert!(!first_line.is_empty());
    assert!(child.wait().unwrap().success());
}

#[test]
fn completions_supported_shells_emit_scripts() {
    for shell in ["bash", "zsh", "fish", "power-shell", "elvish"] {
        run_for(shell);
    }
}

#[test]
fn completions_unknown_shell_errors() {
    Command::cargo_bin("agentstack")
        .unwrap()
        .args(["completion", "tcsh"])
        .assert()
        .failure();
}
