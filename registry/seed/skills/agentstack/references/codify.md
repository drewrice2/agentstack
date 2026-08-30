# Codify what you already have

Read this when the user has prompts or skills scattered around — in
`.claude/skills/`, `.codex/skills/`, a notes folder, or pasted into chat — and
wants to turn them into validated, version-controlled, optionally shareable
context. This is the most common first job. Keep it **local-first**: nothing here
needs a registry token until the final, optional sharing step.

Work in passes. Show the user the inventory before changing anything.

## 1. Discover

Scan the working tree and the usual skill homes:

```bash
agentstack skill scan --json
agentstack skill scan ~/.claude/skills --json
agentstack skill scan ~/.codex/skills --json
```

`skill scan` finds skill directories (a folder with `SKILL.md`). It does not
descend into a single skill — for a known skill use `agentstack skill inspect
<path>`. Also note loose prompt files the user points at; those are raw material,
not skills yet.

Report an inventory: how many valid skills, how many need repair, how many loose
prompts to convert.

## 2. Triage each candidate

For every discovered skill:

```bash
agentstack skill inspect <path>
agentstack skill validate <path> --json
agentstack skill lint <path> --json
agentstack skill security-scan <path>
```

Sort results into three buckets and tell the user the counts:

- **Clean** — passes validate and lint. Leave the content alone.
- **Needs repair** — validate or lint failures. Fix with `references/authoring.md`
  (usually: add frontmatter, align directory name with `name`, single-line
  `description`, remove unsupported top-level files, add missing sections).
- **Loose prompt** — not a skill directory yet. Convert with the "Convert a
  prompt into a skill" steps in `references/authoring.md`.

`security-scan` failures (secrets, tokens, customer data) are blocking — strip
them before anything is committed or shared.

## 3. Repair and convert

Fix one skill at a time, smallest change first, and re-run `validate`/`lint`
until clean. Do not rewrite working content the user did not ask you to change —
codifying is about shape and safety, not rephrasing their expertise.

## 4. Put it under version control

Once skills validate, commit them so changes become diffable and reversible:

```bash
git add <skill-dirs>
git commit -m "Codify <n> skills"
```

For many users this is the finish line: their skills are now structured,
validated, and tracked. Stop here unless they want to organize or share.

## 5. Organize into stacks (optional)

When skills cluster by purpose (review, release, on-call), group them into a
stack so they install as one unit. See `references/stacks-and-teams.md`.

## 6. Share when ready (optional, needs a registry)

To make skills available to teammates: push candidates, have an org admin
approve them, set visibility, and hand teammates a one-command install. For a
folder of already-validated skills, `skill adopt` is the bulk path — it scans,
validates, shows a plan, and pushes the valid skills as candidates; invalid
skills are skipped with reasons and one failed push does not abort the rest:

```bash
agentstack skill adopt ./skills --org <org> --dry-run
agentstack skill adopt ./skills --org <org> --yes
```

See `references/stacks-and-teams.md` for the rollout and `references/govern.md`
for approval and visibility. Until a version is approved, it is a candidate
only — do not describe pushed or adopted skills as live.

## Output for a codify pass

Report the inventory (clean / repaired / converted / blocked-on-secrets), what
was committed, and the single recommended next step (e.g. "build a `review`
stack" or "push to share with your team"). Do not silently skip skills — if you
triaged a subset, say which and why.
