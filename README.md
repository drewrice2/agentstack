# AgentStack

[![CI](https://github.com/drewrice2/agentstack/actions/workflows/ci.yml/badge.svg)](https://github.com/drewrice2/agentstack/actions/workflows/ci.yml)

CLI for reusable AI-agent skills. A skill is a directory rooted on
`SKILL.md`. A stack is a curated set of approved skills. **AgentStack does
not execute agents.**

AgentStack supports two tracks. Track A provides token-free local authoring
and installation. Track B provides optional registry-backed sharing through an
existing registry or the loopback registry in this checkout.

## Agents

Install and follow [`examples/skills/agentstack`](examples/skills/agentstack).
Prefer `--json` and `--no-input`. Never pass a token as a CLI argument.

Working on this repository: [`AGENTS.md`](AGENTS.md).

## Install

Rust 1.88 or newer (`rustup update` if `cargo` is older). The binary lands in
`~/.cargo/bin`; add that directory to `PATH` if `agentstack` is not found.

```sh
git clone https://github.com/drewrice2/agentstack
cd agentstack
cargo install --path . --locked
agentstack --version
```

`bash scripts/build-cli-local.sh` writes a release binary under `dist/`.
Default credentials are a local file. No extra system packages are required
to build.

## First success (Track A: token-free local authoring)

```sh
agentstack doctor
agentstack target setup local --yes
agentstack skill init my-skill \
  --name my-skill \
  --description "Use when reviewing pull requests"
agentstack skill validate ./my-skill
agentstack skill lint ./my-skill
agentstack skill install ./my-skill --target local
agentstack skill show my-skill --target local
```

`local` and repo-scoped targets can register their default path on first
use. User-level `codex` and `claude-code` require
`agentstack target setup` first.

`install` writes receipts (`show` / `update` / `uninstall`). `export` copies
files with no receipts. `skill push` uploads a candidate;
`skill version approve` makes one version current.

## Track B: Share through a local registry (optional)

Use Track B when you need registry-backed sharing and do not already have
working access to an existing registry. You need a checkout of this
repository, Docker Compose, `curl`, and the public `agentstack` CLI on your
`PATH`. Run the commands from the repository root.

The `scripts/local-up.sh` script starts the loopback registry, ensures the
`local` organization exists, and issues a new 30-day token. It writes progress
to stderr and one raw token line to stdout. Configure the public CLI and push a
candidate with the following commands:

```bash
scripts/local-up.sh
agentstack registry use http://127.0.0.1:8080
read -rsp 'Token: ' TOKEN; printf '\n'
printf '%s' "$TOKEN" | agentstack auth login --token-stdin
unset TOKEN
agentstack registry ping --auth
agentstack skill push ./my-skill --org local --scope org
```

The local registry is available only at `http://127.0.0.1:8080`. Rerunning
`scripts/local-up.sh` preserves the `local` organization and issues a new
30-day token. `skill push` creates a candidate; it does not approve the
version. Use `agentstack skill version approve` when an authorized user is
ready to make that version current.

For a full local reset, `docker compose down -v` removes the registry's
Postgres and blob volumes. This is destructive and is never run automatically.

An already-hosted endpoint is also available at
`https://registry.agentstack.gg`; use it only when you already have registry
access and a token.

For headless export of an approved stack, use a token path rather than placing
a token in a command:

```sh
AGENTSTACK_REGISTRY_URL=https://registry.agentstack.gg \
AGENTSTACK_TOKEN_PATH=/run/secrets/agentstack_token \
agentstack stack export YOUR_ORG/YOUR_STACK --out ./skills --json --no-input
```

## Contracts

- [`docs/COMMANDS.md`](docs/COMMANDS.md) — command map, `--json`, env, exits
- [`docs/SKILL_FORMAT.md`](docs/SKILL_FORMAT.md) — skill layout and lint
- [`docs/API_CONTRACT.md`](docs/API_CONTRACT.md) — registry HTTP
