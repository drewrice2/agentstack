# Data Cleaning Sample

## Data Quality Issues

- `signup_date` mixes `YYYY-MM-DD` and `MM/DD/YYYY` formats.
- `region` has inconsistent categories: `US-East`, `us east`, and `East`.
- `customer_id` is missing on 3 rows, which makes deduplication ambiguous.
- `amount_usd` contains negative values that may be refunds or entry errors.

## Proposed Cleaning Plan

1. Preserve the raw export unchanged.
2. Normalize `signup_date` into ISO date format and reject unparseable values to
   a review file.
3. Map known `region` variants through an explicit lookup table.
4. Hold rows without `customer_id` for owner review instead of deduplicating
   them automatically.

## Validation Checks

- Confirm every cleaned date parses as ISO 8601.
- Confirm all region values are in the approved lookup table.
- Reconcile row counts between raw, cleaned, and held-for-review outputs.

## Questions

- Are negative amounts valid refunds?
- Which field is authoritative when duplicate customer records disagree?
