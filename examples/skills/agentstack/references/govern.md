# Govern: approval, visibility, lifecycle, audit

Read this for the registry's shared-state actions: approving versions, changing
visibility, yanking or deprecating, checking blast radius, and reading the audit
trail. Every action here changes what teammates see or install.

This CLI repository does not ship a registry server. If `agentstack auth
whoami` fails, stop and keep working locally. Quote the matching confirmation
sentence and wait for explicit current-turn assent before running any command
below.

## Roles

- **Reader** — list, search, show, export, install visible approved skills and
  stacks; run update checks.
- **Publisher** — reader, plus `skill push` candidate uploads.
- **Org admin** — publisher, plus approval, yanks, deprecations, visibility,
  stack mutation, team management, and audit review.
- **Team admin** — may approve and mutate **team-scoped** skills and stacks for
  their team (a scoped subset of org-admin power).
- **Machine token** — usually env-only read/export/install flows.

Map a 403 to the required role before suggesting fixes. Confirm identity with
`agentstack auth whoami` if it is not already known.

## Check blast radius first

Before approving, yanking, deprecating, or changing visibility, see what a change
touches:

```bash
agentstack skill impact <org>/<skill>
agentstack skill version list <org>/<skill>
agentstack skill version show <org>/<skill>@<version>
```

`skill impact` lists the stacks that depend on a skill — the stacks (and their
installers) a lifecycle change will reach.

## Find what needs approval

```bash
agentstack skill candidates --org <org>
```

`skill candidates` is the approval inbox: it lists non-yanked candidate
versions across visible skills, newest first, each with a copy-ready approve
command. Start here instead of polling `skill version list` per skill.

## Approve and move the current version

```bash
agentstack skill version approve <org>/<skill>@<version>
agentstack skill status <org>/<skill>
```

Approval makes one uploaded version current for unpinned installs and update
checks. Allowed for org_admin/server_admin, and team_admin for team-scoped
skills. It does not rewrite existing local installs.

## Visibility

```bash
agentstack skill visibility show <org>/<skill>
agentstack skill visibility set <org>/<skill> --scope org
```

Scopes are `private`, `org`, `team` (with `--team`). Visibility changes who can
read; it never approves a candidate or changes the current version.

## Yank and deprecate

Both require `--reason`, recorded in audit and status:

```bash
agentstack skill version yank <org>/<skill>@<version> --reason "<reason>"
agentstack skill version deprecate <org>/<skill>@<version> --reason "<reason>"
```

Yank marks a pinned version unsafe for default install/export; deprecate marks it
superseded. Neither removes the version or rewrites existing installs. After a
yank, `agentstack install doctor --target <target>` flags installs still on the
yanked version and prints the right fix command for each (update, uninstall, or
a stack update for stack-owned installs).

## Audit trail

```bash
agentstack audit list --org <org>
agentstack audit show <event-id> --org <org>
agentstack skill audit <org>/<skill>
agentstack stack audit <org>/<stack>
```

`audit list`/`audit show` cover the org-wide trail; `skill audit`/`stack audit`
scope to one resource. After any mutation, report the `audit_event_id` the JSON
output returns, so the change is traceable.

## Confirmation sentences

Quote the relevant sentence and wait for explicit current-turn assent:

- **Approve** — "I am about to approve `<org>/<skill>@<version>`; that makes it
  the current approved version for future installs and update checks, subject to
  visibility; it does not rewrite existing local installs. Should I do this now?"
- **Yank** — "I am about to yank `<org>/<skill>@<version>` with reason
  `<reason>`; that marks the pinned version unsafe for default install/export;
  existing local installs are not automatically changed. Should I do this now?"
- **Deprecate** — "I am about to deprecate `<org>/<skill>@<version>` with reason
  `<reason>`; that marks it superseded; it does not remove it or change existing
  local installs. Should I do this now?"
- **Visibility** — "I am about to change `<ref>` visibility from `<old>` to
  `<new>`; that changes who can read it and does not approve any candidate.
  Should I do this now?"
- **Stack mutation** — "I am about to change stack `<org>/<stack>`; future stack
  installs and updates will resolve using this definition; existing local
  installs change only when users update. Should I do this now?"
- **Remove** — "I am about to remove `<name>` from target `<target>`; that
  deletes managed files and receipts for this target only. Should I do this now?"
