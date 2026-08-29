//! `agentstack completion <shell>` — emit a shell-completion script on stdout.

use std::io::{self, Write};

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::{Shell, generate};

use crate::cli::{Cli, ShellArg};

pub fn run(shell: ShellArg) -> Result<()> {
    let shell: Shell = shell.into();
    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_string();
    let mut output = Vec::new();
    generate(shell, &mut cmd, bin, &mut output);

    match io::stdout().write_all(&output) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err.into()),
    }
}
