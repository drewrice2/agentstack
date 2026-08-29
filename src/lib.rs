//! AgentStack — primitives shared between the `agentstack` binary and the
//! integration tests. The CLI is a thin shell over this library so that
//! command logic can be exercised without spawning a subprocess.

pub mod cache;
pub mod cli;
pub mod commands;
pub mod config;
pub mod credentials;
pub mod error;
mod fs_atomic;
pub mod install;
pub mod installed_scan;
pub mod output;
pub mod package;
pub mod receipt;
pub mod registry;
pub mod skill;
pub mod skill_ref;
pub mod targets;
