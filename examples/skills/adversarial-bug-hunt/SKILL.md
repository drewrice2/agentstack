---
name: adversarial-bug-hunt
description: Use when auditing a codebase for bugs via three sequential subagents — Hunter finds, Skeptic disproves, Referee judges — each in a fresh context.
---

# Purpose

Find real bugs in a codebase by chaining three subagents whose roles are in
tension: Hunter is rewarded for false positives, Skeptic is penalized for
wrongly dismissing real bugs, Referee renders the final verdict. Each
subagent runs in a fresh context so it cannot anchor on its own prior
reasoning.

# When to Use

Use this skill when the user asks for an aggressive bug sweep, security
audit, or adversarial review of a codebase, schema, or database. Trigger
phrases include "find bugs", "audit for issues", "adversarial review", or
"hunt bugs in X".

# Instructions

Drive the workflow with three sequential `Agent` tool calls. Do NOT collapse
the roles into one call — the adversarial framing depends on each agent
seeing only what its role requires. Run them in order; each depends on the
previous output.

1. **Hunter** — Spawn a `general-purpose` subagent. Prompt body: the contents
   of `references/hunter-prompt.md`, followed by the target (paths, schema,
   or scope). Capture the full output verbatim.
2. **Skeptic** — Spawn a fresh `general-purpose` subagent. Prompt body: the
   contents of `references/skeptic-prompt.md`, followed by Hunter's full
   output as the input list. Capture the full output verbatim.
3. **Referee** — Spawn a fresh `general-purpose` subagent. Prompt body: the
   contents of `references/referee-prompt.md`, followed by BOTH Hunter's
   report AND Skeptic's review, clearly labeled.

Pass each prompt verbatim — the scoring rules are load-bearing and shape the
agent's behavior. Do not soften them. The only adaptation per run is the
target description (e.g. "the Rust crate at `src/`", "the schema in
`migrations/`").

If the user prefers a manual copy/paste flow across three Claude Code
sessions instead of subagents, follow the same order but `/reset` between
roles. See `references/README.md` for the prompt index and
`examples/workflow.md` for a concrete run.

# Output

Surface the Referee's verdict list as the deliverable: confirmed bugs first,
ordered by severity, with file/line references where available. Note the
count of dismissed claims. Do not re-summarize Hunter or Skeptic output; the
Referee already did that work.

# Boundaries

This skill orchestrates review; it does not fix bugs. Do not patch code as
part of the run — the Referee's list is the input to a separate fix step. Do
not run the three roles in one subagent call, do not let Hunter or Skeptic
see the Referee's rubric, and do not edit the scoring rules.

# Credit

Method by [@systematicls](https://x.com/systematicls); prompts adapted from
[@danpeguine](https://x.com/danpeguine/status/2029268229030285589).
