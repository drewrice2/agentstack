---
name: pr-review
description: Use when reviewing a pull request, diff, or patch before merge — focus the review on real risk instead of style nits.
---

# Purpose

Help an AI agent review code changes the way a careful senior engineer would:
read the diff in context, prioritize material risks, name concrete fixes, and
say what was *not* checked. The aim is to give reviewers a higher-signal
review that catches real bugs, regressions, and operational risk without
drowning the author in low-value comments.

# When to Use

Use this skill when the user asks for review of:

- a pull request, branch, diff, or patch,
- a stack of related commits before merge,
- a single file rewrite or refactor.

Do not use this skill for greenfield design proposals, RFC discussions, or
high-level architecture review — those need a different shape of feedback.

# Inputs

The agent should expect, and ask for if missing:

- the diff (preferred) or a description of the change,
- the file paths involved and the surrounding code where helpful,
- the test files touched or expected to be touched,
- the issue, bug, or product context that motivated the change.

If only the diff is available, say so in the residual-risk section instead of
inventing context.

# Instructions

1. Read the diff in context. Open nearby code, tests, docs, and the linked
   issue when they're available.
2. Prioritize **material risk**: real bugs, behavioral regressions, missing
   tests, security issues, data loss, deployment/migration risk, and
   operability concerns (logging, metrics, error handling at boundaries).
3. Reference exact files and line numbers when available, and explain *why*
   each issue matters in business or correctness terms — not just "this is
   wrong."
4. Suggest the smallest concrete fix direction. Don't rewrite the PR.
5. Call out **test gaps** explicitly. If the change is untested or only
   happy-path-tested, name the cases that would catch the residual risk.
6. Keep style nits out of the findings list unless they hide a correctness or
   maintenance problem. Group them under a separate "Nits" section if at all.
7. Use `references/checklist.md` as a structured pass before drafting output —
   it is the project's review checklist, not exhaustive.

# Output

Lead with **Findings**, ordered by severity (High / Medium / Low). Each
finding has:

- a one-line summary,
- the file and line(s),
- a concrete fix direction,
- the reason it matters.

After Findings, include:

- **Test gaps** — cases the diff doesn't cover.
- **Residual risk** — what you did not review, and why.
- **Nits** (optional) — style or readability suggestions that did not rise to
  Findings.

If there are no Findings, say so plainly and still list Test gaps and
Residual risk. A "no findings" review is not an automatic approval.

# Boundaries

- Do not rewrite the pull request.
- Do not invent product requirements that aren't supported by the diff,
  tests, docs, or linked issue.
- Do not rubber-stamp risky changes — flag the risk even if the diff "works."
- Do not assert performance characteristics without measurements.
- Do not provide compliance, audit, or legal sign-off.
