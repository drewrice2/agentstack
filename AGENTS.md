# AgentStack

This repository is the `agentstack` CLI. It packages, validates, installs,
and shares portable skill directories. It does not execute agents, pick
models, or run prompts.

## Using the product

Load [`examples/skills/agentstack`](examples/skills/agentstack). That skill
owns the safe command path. Prefer `--json` and `--no-input`. Never pass a
bearer token as a CLI argument.

## Changing this repository

Sources of truth, in order:

1. Live `agentstack --help` and `src/cli.rs` / `src/commands/`
2. [`docs/COMMANDS.md`](docs/COMMANDS.md) — names, JSON, env, exits
3. [`docs/SKILL_FORMAT.md`](docs/SKILL_FORMAT.md) — validate and lint
4. [`docs/API_CONTRACT.md`](docs/API_CONTRACT.md) — registry HTTP
5. [`README.md`](README.md) — first-success path
6. [`examples/skills/agentstack`](examples/skills/agentstack) — agent operation

Keep these distinctions explicit in prose and examples:

- `install` writes receipts; `export` does not
- `skill push` creates a candidate; `skill version approve` makes it current
- `--scope` is visibility, not approval
- `--force` overwrites a local destination; it does not skip auth or hashes
- tokens never appear in URLs, logs, receipts, `config.toml`, or errors
- JSON on stdout; JSON errors on stderr; `--json` never prompts

Do not add a marketplace, workflow engine, agent runtime, or
customer-operated registry server to this repo.

Namespaces are singular and resource-first (`skill`, `stack`, `install`,
`auth`, `registry`, `target`). Product name: AgentStack. Binary, crate, and
env prefix: `agentstack` / `AGENTSTACK_*`.

## Verify

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Docs-only changes do not need a full test run unless they claim command
behavior. CI also runs `cargo deny`.
