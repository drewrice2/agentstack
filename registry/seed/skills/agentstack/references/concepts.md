# Concepts

Read this when the user asks what an AgentStack term means, or when an
explanation hinges on one of the distinctions below.

## Terms

- **Skill** — a portable directory rooted at `SKILL.md` that tells an agent when
  and how to handle one kind of work.
- **Stack** — a curated registry resource that installs approved skills together,
  so a team rolls out consistent context as one unit.
- **Target** — a built-in install destination: `claude-code`, `codex`,
  `repo-claude-code`, `repo-codex`, or `local`.
- **Receipt** — AgentStack's local record of a managed install: what it
  installed, where it came from, the version/hash written, and how to update or
  remove it.
- **Candidate** — an uploaded skill version that exists in the registry but is
  not yet the current approved version. `skill candidates` lists the ones
  awaiting approval.
- **Manifest** — a repo-root `agentstack.toml` declaring skills/stacks per
  target; `agentstack sync` converges installs to it.
- **Overlay** — the matching `platform/<name>/` files applied over an installed
  skill root for `claude-code`/`codex` targets; exports stay verbatim.
- **Approved / current** — the version default installs and update checks resolve
  to, subject to visibility and stack policy.
- **Visibility / scope** — who can read a skill or stack: `private`, `org`, or
  `team`.
- **Export** — an unmanaged copy of skill files with no receipts; useful for CI,
  build folders, and one-off snapshots.
- **Governance** — role-aware control over candidate upload, approval, visibility,
  stack membership, yanks, deprecations, team membership, and audit review.
- **Runtime-agnostic** — AgentStack manages skill files and metadata; it does not
  execute agents, route model calls, or require a single runtime.

## Distinctions that cause most confusion

- **Candidate vs approved** — uploading (`skill push`) does not make a version
  current. Only `skill version approve` does. Do not tell teammates a pushed
  skill is live.
- **Install vs export** — install writes receipts and is managed by `update`,
  `show`, `why`, and `uninstall`. Export just copies files; those four commands
  will not manage it.
- **Visibility vs approval** — visibility controls who can *read*; approval
  controls which version *installs*. Changing one never changes the other.
- **Target vs path** — a target is a named destination; its path is configurable.
  Resolve it with `agentstack target path <target>` instead of guessing.
- **Stack-owned vs direct install** — a skill pulled in by a stack is managed by
  that stack. Update or remove it through the stack, not directly.
