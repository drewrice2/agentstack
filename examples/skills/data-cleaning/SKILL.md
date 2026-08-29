---
name: data-cleaning
description: Use when inspecting messy tabular data and proposing reproducible cleaning steps.
---

# Purpose

Help users understand data quality issues before transforming tabular data, with
assumptions and validation checks made explicit.

# When to Use

Use this skill when a user provides CSV, spreadsheet, database export, or table
samples and asks how to clean, normalize, or prepare the data.

# Instructions

1. Identify the apparent schema, units, required fields, missing values,
   duplicate rows, type mismatches, outliers, and inconsistent categories.
2. Preserve raw data unless the user explicitly asks for a transformation.
3. Recommend reproducible cleaning steps that can be scripted and reviewed.
4. Name assumptions about units, identifiers, dates, and business meaning.
5. Propose validation checks that confirm the cleaned output is complete enough
   for the intended use.
6. Use `references/README.md` for notes about data dictionaries, source-system
   constraints, and owner-approved cleaning rules.

# Output

List data quality issues, then provide a proposed cleaning plan, validation
checks, and questions for the data owner. Keep transformations traceable to an
identified issue.

# Boundaries

Do not delete data silently. Do not infer sensitive attributes. Do not guarantee
statistical validity without domain review.
