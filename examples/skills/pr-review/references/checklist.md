# PR review checklist

A structured pass to make before drafting output. This is not exhaustive —
the goal is to catch the common shapes of risk before they ship.

## Correctness

- Does the new code do what the issue or commit message says it does?
- Are off-by-one, null/empty, and boundary cases handled?
- Are error paths handled (and not just logged-and-continue when they
  shouldn't be)?
- Are concurrency assumptions explicit? Race conditions? Reentrancy?
- Are time, timezone, locale, and unicode concerns considered when relevant?

## Tests

- Are there tests for the new behavior? Both happy-path and at least one
  failure mode?
- Are existing tests still meaningful, or did the diff just patch them to
  pass?
- Do tests exercise the public contract, not just internal helpers?
- Are slow tests gated, or are they fast enough to run on every PR?

## Security & data

- Any new authentication, authorization, or trust boundary?
- Any new place a secret could be logged, returned, or written to disk?
- Any user-controlled input that flows into a query, file path, command, or
  template?
- Are PII / sensitive fields treated consistently with the rest of the
  codebase?

## Operability

- Will this change be safe to deploy without coordination?
- Migration: backwards-compatible? Rollback story?
- Logging / metrics / tracing for the new code path?
- Feature flags, kill switches, or configuration to disable on incident?
- Resource use: new allocations, locks, network calls per request?

## Code shape

- Is the change minimal for the stated goal?
- Are there abstractions added speculatively that no caller needs yet?
- Are public APIs and types named consistently with the rest of the module?
- Are comments documenting *why*, not *what*?

## Documentation

- Public API or CLI changes: are docs / help text / changelog updated?
- Behavior changes a teammate or customer would notice: is that disclosed
  somewhere they will see?

## Out of scope (do not block on)

- Personal style preferences that do not affect correctness or readability.
- Refactoring opportunities unrelated to the diff.
- Hypothetical future requirements that were not asked for.
