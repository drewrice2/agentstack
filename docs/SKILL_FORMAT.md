# Skill Format

A skill in AgentStack is a small directory rooted at a single `SKILL.md`.
Everything else is optional context. The format is deliberately flat,
inspectable, and portable.

## Directory Layout

```
skill-name/
  SKILL.md          # required entry point
  reference.md      # optional: any ordinary support file
  references/       # optional: reference material the agent can cite
  examples/         # optional: concrete examples that anchor behavior
  assets/           # optional: images, fixtures, schema files
  scripts/          # optional: helper scripts the agent can inspect or run
  agents/           # optional: agent-specific config such as openai.yaml
  platform/         # optional: per-platform adaptations
```

`SKILL.md` is the only required file. The five standard subdirectories are
recognized by `agentstack skill inspect`, scanned by `agentstack skill lint`,
and created by `agentstack skill init`, but they are not an exclusive
allowlist. `agentstack skill validate`, `pack`, `unpack`, and `security-scan`
also support ordinary visible support files and directories such as
`LICENSE.txt`, `reference.md`, `templates/`, `agents/`, `python/`, or
`typescript/`. `inspect` reports these as "unknown" so authors can notice
non-standard content, but they remain valid package content.

Packaging skips hidden, junk, and secret-looking files and directories by
default. Archive extraction rejects those excluded entries instead of writing
them to disk.

## SKILL.md Structure

A SKILL.md is a Markdown file with a YAML frontmatter block at the top:

```markdown
---
name: my-skill
description: Use when foo happens and you need to do bar
---

# Purpose

Why this skill exists in one or two sentences.

# When to Use

Concrete triggers. Be specific so the agent can recognize the situation.

# Instructions

What to do, in order.

# Output

What the agent should produce. Format constraints, success criteria.

# Boundaries

What this skill does *not* cover. Edge cases that should defer elsewhere.
```

### Frontmatter

| Field | Required | Rules |
| --- | --- | --- |
| `name` | yes | Slug: lowercase ASCII letters, digits, hyphens; starts with a letter; ≤ 64 chars; no consecutive hyphens; no trailing hyphen. |
| `description` | yes | Single line, non-empty, ≤ 500 chars. Should start with a trigger phrase (`Use when…`, `Use to…`, `Triggers when…`). |

`name` MUST equal the directory name. `description` is shown in search
results and `agentstack skill list`, so optimize it for trigger recognition.

### Recommended Sections

`agentstack skill lint` warns when any of these `# H1` sections are missing:

- `# Purpose`
- `# When to Use`
- `# Instructions`
- `# Output`
- `# Boundaries`

These aren't validation errors — a skill technically remains valid without
them — but their absence usually points at incomplete authoring.

## Validation Rules (Hard)

The full set is enforced by `agentstack skill validate` and lives in
[`src/skill/validate.rs`](../src/skill/validate.rs). Each rule has a stable
snake_case error code so JSON consumers can match on it. Validation errors may
include `position: { "line": N, "col": N }`; both numbers are 1-based SKILL.md
positions.

| Code | Meaning | Position |
| --- | --- | --- |
| `not_a_directory` | Target path is not a directory | none |
| `missing_skill_md` | No `SKILL.md` at the root | none |
| `invalid_utf8` | `SKILL.md` is not valid UTF-8 | none |
| `missing_frontmatter` | No `---` frontmatter block | start of file |
| `invalid_frontmatter` | YAML frontmatter is malformed | parser location when available |
| `missing_name` | `name` field absent or empty | start of file |
| `missing_description` | `description` field absent or empty | start of file |
| `invalid_name` | `name` violates the slug rules | `name:` line |
| `name_mismatch` | `name` does not match the skill directory name | `name:` line |
| `description_too_long` | `description` exceeds 500 chars | `description:` line |
| `description_multiline` | `description` contains line breaks; it must be a single line | `description:` line |
| `unsupported_top_level_entry` | Top-level entry is a symlink or other non-regular filesystem entry | none |
| `io_error` | Filesystem I/O failed while reading the skill | none |

## Lint Rules (Soft)

Documented in [`src/skill/lint.rs`](../src/skill/lint.rs). Each warning has
a stable snake_case code:

`vague_description`, `non_trigger_description`, `missing_section_purpose`,
`missing_section_when_to_use`, `missing_section_instructions`,
`missing_section_output`, `missing_section_boundaries`,
`no_examples_directory`, `no_references_directory`, `skill_md_too_long`,
`unreferenced_reference`, `placeholder_content`.

## Packaging

`agentstack skill pack` produces a deterministic `.tar.gz`:

- Top-level entry equals the skill name
- Archive content includes `SKILL.md` plus ordinary visible support files and
  directories, including but not limited to `references/`, `examples/`,
  `assets/`, `scripts/`, `agents/`, `platform/`, `templates/`, language
  folders, root `LICENSE.txt`, and root Markdown support files
- Existing standard directories are preserved, even when they are empty
- Hidden files and secret-looking files are skipped by default
- Symlinks and other non-regular filesystem entries are excluded and reported
- Archive entries are sorted by archive path
- mtimes are zeroed; ownership is dropped (uid/gid `0`)
- gzip header carries a fixed OS byte (`0xff`)
- Conservative archive limits cap archive bytes, entry count, per-file
  extracted bytes, and total extracted bytes

Repacking the same source on the same machine produces byte-identical
output, which lets the registry cache by hash and the CLI verify on export.

The package SHA-256 is the authoritative content identifier. It is stored
on every `SkillMetadata` record and re-checked by `agentstack skill export`
before unpacking.

## Platform Adaptations

When a skill needs platform-specific tweaks (e.g. a different system
prompt for Claude Code vs. Codex), put them under `platform/<name>/`:

```
my-skill/
  SKILL.md
  platform/
    claude-code/
      SKILL.md       # overrides
    codex/
      example.md
```

`agentstack skill pack` includes everything under `platform/` verbatim, and
`agentstack skill export` writes it verbatim. At install time the directory
becomes an overlay: installing or updating into a `claude-code` or `codex`
target (including the repo-scoped variants) copies `platform/<name>/` files
over the installed skill root, so a platform file replaces the base file at
the same relative path. The `platform/` directory itself stays in place, and
the `local` target gets no overlay. Receipt content hashes reflect the
post-overlay contents.

Tag the skill with platform metadata at publish time using `--platform`
(repeat for multiple platforms):

```
agentstack skill push ./my-skill --org acme --platform claude-code --platform codex
```

Platform tags surface in search, list, and version output so consumers
can filter to skills that target their environment.

## Stability

The format is pre-1.0. New optional fields and lint codes can be added
without bumping the major version; existing required fields and
validation codes are stable.
