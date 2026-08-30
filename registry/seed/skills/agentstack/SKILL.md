---
name: agentstack
description: Use when a user wants to codify, version, install, update, share, or govern AgentStack skills and stacks — turning prompts into validated skills, building and rolling out stacks to a team, or inspecting what context is installed.
---

# Purpose

AgentStack is a CLI with an optional local registry for reusable AI agent
skills and stacks. It validates, packages, installs, updates, shares, and
governs portable context. It does not execute agents, choose models, or run
prompts.

This skill makes you a careful AgentStack operator: read the user's goal, take
the smallest safe command path, verify the result, and describe what changed in
AgentStack terms. It works the same whether you are Claude Code, Codex, or
another runtime — only the install target differs.

# When to Use

Use this when the user wants to organize, version, install, update, inspect,
share, or govern agent skills or stacks — or says things like "turn this prompt
into a skill," "audit my skills," "roll this out to my team," or "what context
is installed?"

Also use it when the working tree contains `SKILL.md`, `.claude/skills/`,
`.codex/skills/`, `.agentstack-install.json`, or `.agentstack-stacks/`.

Do not use it for model choice, prompt execution, or agent runtime behavior.
AgentStack manages portable context; it is not an agent runtime, prompt
marketplace, or eval system.

# Instructions

`agentstack <command> --help` is the source of truth for exact flags — check it
before running anything user-visible. This skill owns the mental model and the
safe path, not a flag dump. Prefer `--json` for inspection; summarize the fields
that matter.

## Mental model

- **Skill** — a portable directory rooted at `SKILL.md` that tells an agent when
  and how to do one kind of work.
- **Stack** — a curated set of approved skills installed together as one unit.
- **Target** — where AgentStack installs context: `claude-code`, `codex`, their
  `repo-` variants, or `local`.
- **Receipt** — the local record of a managed install: source, version, hash,
  destination. Powers `show`, `update`, `why`, and `uninstall`.
- **Candidate vs approved** — `push` uploads a candidate; only an *approved*
  version is what default installs and update checks resolve to.
- **Install vs export** — install writes receipts and is managed; export copies
  files with no receipts (for CI and snapshots).
- **Visibility/scope** — who can read a skill or stack (private, org, team). It
  does not approve a candidate or change the current version.
- **Manifest** — a repo-root `agentstack.toml` declaring skills/stacks per
  target; `agentstack sync` converges installs to it.
- **Overlay** — matching platform files (e.g. `platform/claude-code/`) applied
  over the installed root for `claude-code`/`codex` targets; exports stay
  verbatim.

## Choose a track

Use Track A for authoring, validation, linting, and local installation; it
needs no token. Use Track B for registry-backed sharing when the user has an
authenticated registry or explicitly requests the optional local registry
from this checkout. The main skill's Track A/Track B decision rule determines
which path to use. The README owns the local registry bootstrap sequence; do
not copy those commands here.

Authenticate first before routing any registry mutation. Without registry
access, apply the decision rule and continue with Track A unless the user
explicitly requests Track B.

## Find the user's goal

Match the user's intent to one playbook and open only that reference. Most work
starts local and needs no registry token; registry-backed and team steps are
opt-in.

| The user wants to… | Start with | Reference |
| --- | --- | --- |
| Organize or audit prompts/skills they already have | `agentstack skill scan` | `references/codify.md` |
| Write or fix one skill | `agentstack skill init` | `references/authoring.md` |
| Install, update, or inspect managed context | `agentstack install list --json` | `references/install.md` |
| Keep a repo converged to a declared skill set | `agentstack sync --check` | `references/install.md` |
| Build a stack and share it with a team | Apply the track decision rule; authenticate before mutation | `references/stacks-and-teams.md` |
| Approve, set visibility, yank, audit, or check blast radius | Apply the track decision rule; authenticate before mutation | `references/govern.md` |
| Recover from a failed command | `agentstack doctor` | `references/troubleshooting.md` |
| Know what an AgentStack term means | — | `references/concepts.md` |

## No stated intent? Orient first

If the user has not stated a goal, run read-only discovery, then route:

```bash
agentstack registry show
agentstack install list --json
agentstack target list
agentstack doctor
```

Report only decision-making facts — active registry, login status, managed
installs, usable targets, doctor warnings — and offer one concrete next move
(codify existing skills, install an approved stack, or inspect receipts).

For a Track A learning loop, scan, validate, and install the example skill into
the `local` target (see `references/install.md`); it exercises shape,
validation, receipts, and the update/remove lifecycle without registry access.

# Output

For operational work, report: what ran, what changed, what was verified, what is
pending, and the next command only when it is actionable.

Use AgentStack vocabulary precisely. Say "candidate uploaded," not "published
live"; "managed install receipt," not "copied files"; "exported snapshot," not
"installed," when no receipt was written.

For read-only orientation, keep it shorter: identity/registry, managed state,
relevant warnings, and the recommended next move.

# Boundaries

- Never pass a bearer token as a CLI argument. Pipe it to `auth login` for human
  login, or use `AGENTSTACK_TOKEN_PATH` (with `AGENTSTACK_TOKEN` as fallback) for
  headless commands.
- Never approve, yank, deprecate, change visibility, mutate a stack, widen
  scope, or remove an install without explicit current-turn assent. Use the
  confirmation sentences in `references/govern.md`.
- Never use `--force` to bypass a refusal you do not understand.
- Never update or remove a skill the user is actively editing.
- Never put secrets in skills, docs, logs, shell history, or prompt examples.
- Never imply AgentStack executes agents or guarantees runtime behavior.
