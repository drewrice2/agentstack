# Changelog

All notable changes to AgentStack will be documented in this file.

This project does not have a published release history yet.

## Unreleased

- Tell agents this repository has no registry server; no token means stay
  local.
- Pin CI to read-only contents and document the cargo-deny license allowlist.
- Recheck stack destinations at commit time so a destination that appears
  during installation is not overwritten without `--force`.
- Refuse to follow symlinks in stack receipt roots, organization directories,
  and stack directories.
- Pack archive files without following leaf symlinks replaced after scanning.
- Limit archive reads during unpack before materializing oversized files.
- Remove private fixture identities and clarify credential ignores, example-org
  help, and registry stack authentication guidance.
- Lead the CLI, README, and operator skill with local authoring.
- Collapse overlapping guides. Agents load `examples/skills/agentstack`;
  `docs/COMMANDS.md` is the contract.
- Remove pre-release terminology and align public examples with CLI contracts.
- Default `cargo install` no longer needs Linux D-Bus libraries.
- `doctor` no longer tells a fresh local install to log in first, create a
  config file, or register user-level Claude Code / Codex targets.
- `doctor` treats a missing cache under a creatable config dir as ok, and
  warns on unconfigured `claude-code` / `codex` only when those directories
  already exist.
- README install is a clone-then-`cargo install --path . --locked` path.
  CI also checks the claimed Rust 1.88 MSRV.
