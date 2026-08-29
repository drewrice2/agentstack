# SQL Review Sample

## Findings

1. High - `migrations/202605_add_status.sql` updates every row in `orders`
   before adding the `NOT NULL` constraint. On a large table, that can hold
   locks long enough to block writes.

   Safer shape: add the nullable column, backfill in bounded batches, verify no
   nulls remain, then add the `NOT NULL` constraint in a separate migration.

2. Medium - The query in `reports/revenue.sql` filters on `created_at` but the
   available index starts with `customer_id`. If this report scans a broad date
   range, the current index may not support it well.

## Open Questions

- Which database dialect and version is this targeting?
- Can you share `EXPLAIN` output for the reporting query on production-sized
  data?
- What is the rollback plan if the status backfill is interrupted?
