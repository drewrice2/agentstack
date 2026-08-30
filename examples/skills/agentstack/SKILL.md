---
name: agentstack
description: Use when a user wants to codify, version, install, update, share, or govern AgentStack skills and stacks — turning prompts into validated skills, installing context locally, rolling out stacks to a team, or inspecting what is installed.
---

# Purpose

AgentStack is a CLI for reusable AI-agent skills and stacks. It supports
token-free local authoring and an optional loopback registry for
registry-backed sharing, approval, visibility, stacks, and audit. AgentStack
does not execute agents, choose models, or run prompts.

Read the user's goal, take the smallest safe command path, verify, and
describe what changed in AgentStack terms. Only the install target changes
across runtimes.

# When to Use

Use this when the user wants to organize, version, install, update, inspect,
share, or govern agent skills or stacks — or says things like "turn this
prompt into a skill," "what context is installed?," or "roll this out to my
team."

Also use it when the working tree contains `SKILL.md`, `.claude/skills/`,
`.codex/skills/`, `.agentstack-install.json`, or `.agentstack-stacks/`.

Do not use it for model choice, prompt execution, or agent runtime behavior.

# Instructions

`agentstack <command> --help` is the source of truth for flags. Check it
before running anything user-visible. This skill owns the mental model and
the safe path, not a flag dump. Prefer `--json` for inspection; summarize
the fields that matter.

## Mental model

Details: `references/concepts.md`.

- **Skill** — a directory rooted at `SKILL.md`.
- **Stack** — approved skills installed together as one unit.
- **Target** — install destination: `local`, `claude-code`, `codex`, or
  their `repo-` variants.
- **Receipt** — local record of a managed install. Powers `show`, `update`,
  `why`, and `uninstall`.
- **Candidate vs approved** — `push` uploads a candidate; only approve
  makes a version current.
- **Install vs export** — install is managed; export copies files with no
  receipts.
- **Visibility** — who can read (`private`, `org`, `team`). It does not
  approve a version.

## First success (Track A)

When the user is authoring, validating, linting, or installing locally, use
this token-free loop:

```bash
agentstack doctor
agentstack skill init my-skill --name my-skill --description "Use when ..."
agentstack skill validate ./my-skill
agentstack skill lint ./my-skill
agentstack target setup local --yes
agentstack skill install ./my-skill --target local
agentstack skill show my-skill --target local
```

The bundled copy of this skill is also a valid install:

```bash
agentstack skill install examples/skills/agentstack --target local
```

## Choose a track

Use this decision rule:

- Use Track A for authoring, validation, linting, and local installation.
- If an existing registry authenticates, use it for registry-backed sharing.
- If the user explicitly requests a local registry from this checkout, follow
  the Track B procedure in `README.md`.
- Otherwise, continue with Track A.

`README.md` owns the complete Track B bootstrap sequence. Do not copy that
sequence into this skill or its references.

Before registry mutation, authenticate first. If authentication fails, apply
the decision rule above and do not run registry mutation commands until a
registry authenticates.

## Find the user's goal

Match intent to one playbook and open only that reference. Most work starts
local.

| The user wants to… | Start with | Reference |
| --- | --- | --- |
| Organize prompts or skills they already have | `agentstack skill scan` | `references/codify.md` |
| Write or fix one skill | `agentstack skill init` | `references/authoring.md` |
| Install, update, or inspect managed context | `agentstack install list --json` | `references/install.md` |
| Keep a repo converged to a declared set | `agentstack sync --check` | `references/install.md` |
| Build a stack and share it with a team | Apply the track decision rule; authenticate before mutation | `references/stacks-and-teams.md` |
| Approve, yank, audit, or check blast radius | Apply the track decision rule; authenticate before mutation | `references/govern.md` |
| Recover from a failed command | `agentstack doctor` | `references/troubleshooting.md` |
| Know what a term means | — | `references/concepts.md` |

## No stated intent? Orient first

Run read-only discovery, then route:

```bash
agentstack doctor
agentstack skill scan --json
agentstack install list --json
agentstack target list
```

If the user already uses a registry, also run `agentstack registry show`
and `agentstack auth whoami`. Report only decision-making facts and one
next move.

# Output

For operational work, report: what ran, what changed, what was verified,
what is pending, and the next command only when it is actionable.

Say "candidate uploaded," not "published live"; "managed install receipt,"
not "copied files"; "exported snapshot," not "installed," when no receipt
was written.

For read-only orientation: identity/registry if any, managed state,
relevant warnings, and one next move.

# Boundaries

- Never pass a bearer token as a CLI argument. Pipe it to `auth login` for
  humans, or use `AGENTSTACK_TOKEN_PATH` (`AGENTSTACK_TOKEN` as fallback)
  for headless commands.
- Never approve, yank, deprecate, change visibility, mutate a stack, widen
  scope, or remove an install without explicit current-turn assent. Use the
  confirmation sentences in `references/govern.md`.
- Never use `--force` to bypass a refusal you do not understand.
- Never update or remove a skill the user is actively editing.
- Never put secrets in skills, docs, logs, shell history, or prompt
  examples.
- Never imply AgentStack executes agents or guarantees runtime behavior.
- Never expose the local registry beyond loopback, describe it as
  production-ready or production self-hosting, reset its volumes without
  assent, or expose its token.
