# AgentStack

[![CI](https://github.com/drewrice2/agentstack/actions/workflows/ci.yml/badge.svg)](https://github.com/drewrice2/agentstack/actions/workflows/ci.yml)

CLI for reusable AI-agent skills. A skill is a directory rooted on
`SKILL.md`. A stack is a curated set of approved skills. **AgentStack does
not execute agents.**

Local authoring needs no registry. A private registry adds versions,
approval, visibility, stacks, and audit.

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

## First success

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

## Team registry (optional)

Local authoring does not need this. You need an org on a registry and a
token. This repository is the CLI only — it does not include a registry
server, and there is no self-hosted setup here. The default URL is
`https://registry.agentstack.gg`. Loopback HTTP
(`agentstack registry use http://127.0.0.1:8080`) works if you already run
a compatible registry.

Humans: `agentstack auth login`. Agents and CI: `AGENTSTACK_TOKEN_PATH`
(or `AGENTSTACK_TOKEN`). Then use `org/name` refs for that org — not the
`acme` examples unless that is your org.

```sh
agentstack registry ping --auth
agentstack skill push ./my-skill --org YOUR_ORG --scope org
```

Headless export of an approved stack:

```sh
AGENTSTACK_REGISTRY_URL=https://registry.agentstack.gg \
AGENTSTACK_TOKEN_PATH=/run/secrets/agentstack_token \
agentstack stack export YOUR_ORG/YOUR_STACK --out ./skills --json --no-input
```

## Contracts

- [`docs/COMMANDS.md`](docs/COMMANDS.md) — command map, `--json`, env, exits
- [`docs/SKILL_FORMAT.md`](docs/SKILL_FORMAT.md) — skill layout and lint
- [`docs/API_CONTRACT.md`](docs/API_CONTRACT.md) — registry HTTP
