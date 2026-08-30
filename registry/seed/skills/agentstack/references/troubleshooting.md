# Troubleshooting

Read this after an `agentstack` command fails. Capture the exact command, exit
status, stderr, any JSON error fields, and any `hint` or `next_command`.

## Start here

```bash
agentstack doctor
agentstack registry show
agentstack config show
agentstack target list
```

`doctor` checks config, cache, auth, registry, targets, and receipts. It always
exits 0 — read the summary instead of treating exit 0 as "everything is fine."
`config show` prints the resolved configuration when a path or override is in
doubt.

## Auth and registry

401, missing token, or invalid token:

```bash
agentstack registry ping --auth
printf '%s' "$AGENTSTACK_TOKEN" | agentstack auth login
agentstack auth whoami
```

Never pass tokens as command arguments. If the token is expired, the user or
admin must issue a new one. `auth status` is the local, no-network check;
`auth whoami` and `registry ping --auth` confirm remote identity.

403:

1. Confirm identity with `auth whoami` unless output already names it.
2. Map the command to a role (reader, publisher, org admin, or team admin) — see
   `references/govern.md`.
3. Inspect status or visibility if readable:

```bash
agentstack skill status <org>/<skill>
agentstack skill visibility show <org>/<skill>
agentstack stack status <org>/<stack>
```

Likely fixes: role grant, team membership, visibility change, approval, or a
corrected ref. Do not call a real 403 a login problem unless identity is wrong.

404 or not found:

```bash
agentstack skill search <query> --org <org>
agentstack skill version list <org>/<skill>
agentstack stack list --org <org>
```

`org/skill` means the current approved version; use `org/skill@version` for a
pinned uploaded version.

## Pushed but teammates cannot install it

A pushed skill is a candidate until approved:

```bash
agentstack skill version list <org>/<skill>
agentstack skill status <org>/<skill>
```

Org/team admin fix: approve it (see `references/govern.md`). Do not describe
`skill push` as live for teammates.

## Validate or lint failure

```bash
agentstack skill inspect ./my-skill
agentstack skill validate ./my-skill --json
agentstack skill lint ./my-skill --json
```

Common validation fixes: add root `SKILL.md`, add frontmatter, align directory
name with `name`, keep `description` single-line, remove unsupported top-level
files. Common lint fixes: make the description trigger-like, add missing H1
sections, remove placeholder text, link every file under `references/`, and move
overlong background out of `SKILL.md`. See `references/authoring.md`.

## Target or receipt problems

No usable target:

```bash
agentstack target detect
agentstack target setup <target> --yes
agentstack target path <target>
```

Local/path install cannot update — reinstall from the registry source first:

```bash
agentstack skill install <org>/<skill> --target <target> --force
agentstack skill update <skill> --target <target> --check
```

Skill required by a stack — manage it through the stack:

```bash
agentstack install why <skill> --target <target>
agentstack skill impact <org>/<skill>
agentstack stack update <org>/<stack> --target <target> --check
```

Corrupt receipts or stale locks:

```bash
agentstack install doctor --target <target>
agentstack install unlock --target <target>
```

Use `install unlock` only for a genuinely stale lock, and explain the
consequence first.

Installed version yanked or stale: `install doctor` also runs registry
lifecycle checks when reachable and authed. `installed_version_yanked` is a
failure with the yank reason and the right fix command for the install:
update when a replacement version exists, uninstall when none does, or a
stack update for stack-owned installs. Run the command doctor prints, e.g.:

```bash
agentstack skill update <skill> --target <target>
```

## Refusing to overwrite

`--force` only controls replacement. Use it after confirming intent and
understanding the existing managed state — never to bypass a refusal you do not
understand:

```bash
agentstack skill install ./my-skill --target codex --force
agentstack stack install <org>/<stack> --target codex --force
agentstack skill pack ./my-skill --out ./my-skill.tar.gz --force
```
