# Agent Instructions Audit Sample

## Must Fix

1. High - `AGENTS.md` tells agents to "use all available tools" without naming
   approval boundaries. That can conflict with local rules that require
   confirmation before shared-state or destructive actions.

   Suggested wording: "Use local, reversible tools without confirmation. Ask
   before destructive actions or changes to shared external systems."

2. Medium - The prompt asks for tests but does not define what evidence counts
   as verified. Add command-level success criteria for validation, lint, and
   runtime checks.

## Polish

- The trigger section could be shorter. The first paragraph should state when
  the instruction file applies.

## Open Questions

- Are agents allowed to create external tickets or pull requests, or should
  they draft those changes for a human to submit?
