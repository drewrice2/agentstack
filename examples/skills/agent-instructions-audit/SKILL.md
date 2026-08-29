---
name: agent-instructions-audit
description: Use when reviewing agent instruction files or prompts for clarity, scope, and operational safety.
---

# Purpose

Review agent-facing instructions for clear triggers, bounded authority,
consistent priorities, and verifiable operating rules.

# When to Use

Use this skill when a user asks to review `AGENTS.md`, `CLAUDE.md`, skill files,
system prompts, tool instructions, or project-specific agent guidance.

# Instructions

1. Check whether triggers explain when the instruction applies and when it does
   not.
2. Identify conflicting instructions, unsafe authority, missing verification
   steps, unclear ownership, and weak secret-handling rules.
3. Separate must-fix operational risks from wording polish.
4. Prefer minimal edits that create clearer decision points.
5. Preserve project-specific constraints unless the user explicitly asks to
   change them.
6. Use `references/README.md` for notes about local policy, tool boundaries,
   and examples of acceptable instruction wording.

# Output

Lead with high-risk instruction issues. Include suggested wording changes when
they are clear and bounded. End with open questions for the instruction owner.

# Boundaries

Do not broaden agent permissions. Do not add tool capabilities that do not
exist. Do not remove project-specific constraints unless explicitly asked.
