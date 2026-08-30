---
name: minimal-skill
description: Use when you need a tiny AgentStack skill template that validates.
---

# Purpose

Show the smallest practical AgentStack skill shape an agent can copy and adapt.

# When to Use

Use this template when creating a new skill from scratch or checking whether a
minimal `SKILL.md` has the required frontmatter and recommended sections.

# Instructions

1. Rename the directory and `name` field to the new skill slug.
2. Replace the description with a trigger-oriented sentence.
3. Replace these sections with task-specific instructions.
4. Read `references/checklist.md` before publishing.
5. Use `examples/request.md` as a tiny input example.
6. Run `agentstack skill validate <path>` and `agentstack skill lint <path>`.

# Output

Produce a valid skill directory rooted at `SKILL.md`.

# Boundaries

Do not use this as final domain guidance. It is only a starter template.
