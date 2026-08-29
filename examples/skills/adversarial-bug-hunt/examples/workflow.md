# Example Workflow

A condensed three-session run on a hypothetical Postgres schema audit.

## Session 1 — Hunter

User pastes `hunter-prompt.md`, then attaches the schema. Hunter returns:

```
1. Location: users.email
   Description: No UNIQUE constraint; duplicates possible.
   Impact: Medium
   Points: 5

2. Location: orders.total_cents
   Description: Stored as FLOAT; rounding errors on aggregation.
   Impact: Critical
   Points: 10

3. Location: sessions.token
   Description: No expiry column; sessions live forever.
   Impact: Medium
   Points: 5

Total score: 20
```

Save the full output. `/reset`.

## Session 2 — Skeptic

User pastes `skeptic-prompt.md`, then appends Hunter's output. Skeptic returns:

```
Bug 1 (5 pts):
  Counter: Application enforces uniqueness via ORM validator.
  Confidence: 60%
  Decision: ACCEPT (DB-level constraint still missing; 60% × 5 < 40% × 10)
  Risk: 0 / -10

Bug 2 (10 pts):
  Counter: Verified — column type is FLOAT, this is real.
  Confidence: 5%
  Decision: ACCEPT
  Risk: 0 / -20

Bug 3 (5 pts):
  Counter: sessions.created_at + app-side TTL handles expiry.
  Confidence: 85%
  Decision: DISPROVE
  Gained: +5

Disproved: 1 | Accepted: 2 | Score: +5
```

Save the full output. `/reset`.

## Session 3 — Referee

User pastes `referee-prompt.md`, then appends both prior outputs. Referee
returns:

```
Bug 1: REAL BUG (High confidence)
  ORM validators are bypassable; DB constraint is the source of truth.

Bug 2: REAL BUG (High confidence)
  FLOAT for money is a known data-integrity hazard.

Bug 3: NOT A BUG (Medium confidence)
  App-side TTL is a defensible design choice if documented.

Confirmed: 2 | Dismissed: 1
- users.email — missing UNIQUE (Medium)
- orders.total_cents — FLOAT for currency (Critical)
```

The Referee's "Confirmed" list is the deliverable.
