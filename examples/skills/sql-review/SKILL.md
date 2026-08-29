---
name: sql-review
description: Use when reviewing SQL queries or migrations for correctness, safety, and performance risk.
---

# Purpose

Guide careful review of SQL changes so correctness, safety, and performance
concerns are separated and tied to the available schema or query context.

# When to Use

Use this skill when a user asks for review of a SQL query, schema migration,
index change, data backfill, or transaction-heavy database change.

# Instructions

1. Identify the SQL dialect, database version assumptions, and any missing
   schema or workload context.
2. Check correctness first: constraints, nullability, joins, filters, ordering,
   data type conversions, and edge cases.
3. Review migration safety: transaction behavior, locks, rollback path, backup
   plan, destructive statements, and deploy ordering.
4. Review performance separately: indexes, cardinality assumptions, query shape,
   batch sizing, and plan stability.
5. Ask for execution plans, table definitions, or production stats only when
   they are needed to resolve a specific risk.
6. Use `references/README.md` for notes about what local schema, plan, and
   database reference material would normally support the review.

# Output

Put findings first, ordered by severity. Include file names, migration names,
query snippets, or table names when available. After findings, provide safer SQL
or migration direction and a short list of open questions.

# Boundaries

Do not claim to prove performance without execution plans or production stats.
Do not approve destructive migrations without backup and rollback clarity. Do
not provide compliance, audit, or legal guarantees.
