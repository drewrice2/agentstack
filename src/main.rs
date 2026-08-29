use std::process::ExitCode;

use agentstack::cli::Cli;
use agentstack::commands;
use agentstack::output::{Ctx, render_clap_error_json, render_error};
use clap::Parser;
use clap::error::ErrorKind;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) || !argv_has_json()
            {
                err.exit();
            }
            render_clap_error_json(&err);
            return ExitCode::from(err.exit_code() as u8);
        }
    };
    let ctx = Ctx::from_global(&cli.global);
    match commands::dispatch(cli, &ctx) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            render_error(&err, ctx.json);
            ExitCode::FAILURE
        }
    }
}

fn argv_has_json() -> bool {
    std::env::args_os().any(|arg| arg == "--json")
}
