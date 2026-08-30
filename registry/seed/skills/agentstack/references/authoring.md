# Authoring one skill

Read this when creating, repairing, or reviewing a single skill directory. Write
for the agent that will load the skill, not for a human browsing docs. To
organize a whole pile of existing prompts and skills at once, read
`references/codify.md` instead — it uses this file per skill.

## Valid shape

```text
skill-name/
  SKILL.md
  references/
  examples/
  assets/
  platform/
```

Only `SKILL.md` is required. The other directories are optional; any visible
top-level entry outside this set fails validation. Platform-specific adaptations
belong under `platform/`, not in the generic skill.

`SKILL.md` starts with YAML frontmatter:

```markdown
---
name: code-review
description: Use when reviewing pull requests for correctness and regression risk.
---
```

Hard validation rules:

- `name` is a lowercase ASCII slug, at most 64 characters, with no trailing or
  consecutive hyphens.
- `name` matches the directory name.
- `description` is one non-empty line, at most 500 characters.
- `SKILL.md` is UTF-8 with valid YAML frontmatter.

## Build a useful skill

1. Name the trigger: what user wording, files, APIs, or failures should load the
   skill? Put it in the `description` — that is what an agent matches on.
2. Name exclusions: what similar work is out of scope?
3. Use these H1 sections unless there is a strong reason not to: `Purpose`,
   `When to Use`, `Instructions`, `Output`, `Boundaries`.
4. Keep `SKILL.md` lean. Move tables, schemas, long command maps, samples, and
   checklists into `references/` or `examples/`, and link each from `SKILL.md`.
5. Write ordered actions with observable checks. Avoid generic reminders that do
   not change behavior.

`examples/minimal-skill/` in this skill is the smallest shape you can copy and
adapt.

## Convert a prompt into a skill

Treat the prompt as source material, not text to paste:

```bash
agentstack skill init <slug> --name <slug> --description "Use when ..."
```

Then rewrite:

- convert human-facing prose into agent-facing instructions;
- remove secrets, tokens, customer data, and one-off local paths;
- split background into `references/`;
- put realistic, safe inputs in `examples/`;
- link every reference from `SKILL.md`.

## Verify before sharing

```bash
agentstack skill validate ./<slug>
agentstack skill lint ./<slug>
agentstack skill security-scan ./<slug>
agentstack skill inspect ./<slug>
```

`validate` enforces hard structure rules; `lint` flags quality issues;
`security-scan` looks for secrets and unsafe content; `inspect` summarizes what
an agent will load.

## Publish handoff (optional, when sharing)

Ask for org and visibility first. Keep private by default:

```bash
agentstack skill push ./<slug> --org <org>
agentstack skill status <org>/<slug>
```

`push` uploads a **candidate** — not a current version. Report it that way. Give
an org admin the approval command only after `skill status` or push output names
the version (see `references/govern.md`):

```bash
agentstack skill version approve <org>/<slug>@<version>
```

## Quality checklist

- Trigger and exclusions are concrete.
- Instructions say what to do, in what order, with observable checks.
- Output format matches the work.
- Boundaries prevent unsafe registry, filesystem, security, or runtime claims.
- No secrets, credentials, API keys, or raw tokens appear in any file.
- Every file under `references/` is linked from `SKILL.md`.
- `validate` is clean; `lint` warnings are fixed or explicitly accepted.
