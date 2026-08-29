# Build a stack and share it with a team

Read this when the user wants to group skills into a stack and roll it out —
solo, to a team, or across a company. Creating or sharing a stack requires a
registry token and a publisher/admin role. Installing a registry stack
requires a token that can read that stack. Treat every mutation as shared
state and confirm before running it.

## Build a stack

A stack is a curated set of approved skills. Create it, then add members:

```bash
agentstack stack create <org>/<stack> --scope private
agentstack stack add <org>/<stack> <org>/<skill>
agentstack stack show <org>/<stack>
```

`--scope` is `private` (default), `org`, or `team` (with `--team <team>`). Use
`--name` and `--description` to make the stack self-explanatory.

A stack child only resolves if its skill version is approved and visible. Choose
a version policy per child:

```bash
# Track whatever is approved/current (default):
agentstack stack add <org>/<stack> <org>/<skill> --version-policy current

# Pin to an exact version that will not move:
agentstack stack add <org>/<stack> <org>/<skill>@<version> \
  --version-policy pinned --pin-version <version>
```

Use `current` for skills you want teammates to receive updates for; `pinned`
when a stack must stay reproducible. Remove a child with
`agentstack stack remove <org>/<stack> <org>/<skill> --yes` (preview with
`--dry-run`; without `--yes` the command asks for confirmation).

## Choose who can see it

Visibility controls read access; it does not approve anything:

```bash
agentstack stack visibility show <org>/<stack>
agentstack stack visibility set <org>/<stack> --scope org
```

Scopes: `private` (you), `org` (everyone in the org), `team` (one team, with
`--team`). Confirm before widening scope — see `references/govern.md`.

## Set up a team

For team-scoped rollout, create the team and assign roles. `--role` is required
on member commands and is `member` or `team_admin`:

```bash
agentstack team create <org>/<team>
agentstack team add-member <org>/<team> user@example.com --role member
agentstack team add-member <org>/<team> lead@example.com --role team_admin
agentstack team inspect <org>/<team>
```

Manage membership with `team set-role <org>/<team> <email> --role <role>`,
`team remove-member <org>/<team> <email>`, and `team list --org <org>`. A
`team_admin` can approve and mutate team-scoped skills and stacks; a `member`
consumes them. Confirm membership and role changes before running them.

## Hand teammates one command

Once the stack is approved and visible, onboarding a teammate's machine is a
single install. They set up a target once, then install the stack:

```bash
agentstack registry show
printf '%s' "$AGENTSTACK_TOKEN" | agentstack auth login
agentstack target setup codex --yes        # or claude-code, or a repo- target
agentstack stack install <org>/<stack> --target codex
agentstack stack show <org>/<stack> --target codex
```

The same stack installs into Claude Code or Codex by changing only the target.
For repo-local context, use `repo-codex` or `repo-claude-code` from the repo
root — or commit a repo-root `agentstack.toml` declaring the stack so teammates
just run `agentstack sync` after cloning (see `references/install.md`). See
`references/install.md` for targets, receipts, and updates.

## Personas

- **Solo / version control** — skip teams and the registry; keep skills local and
  in git (`references/codify.md`).
- **Team share** — build a stack, set `--scope team`, create the team, hand the
  one-command install above.
- **Company rollout** — `--scope org`, pin critical children, and lean on
  `references/govern.md` for approval, audit, and blast-radius checks.
