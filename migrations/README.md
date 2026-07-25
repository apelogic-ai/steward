# Migrations

SQL migrations are append-only. Slice S3 introduces Postgres operational state
for immutable envelope revisions, admission decisions, the approval queue, and
runtime-event history.

`cargo xtask migrate-check` rejects edits or renames of migrations already
present on the comparison base. The S3 store integration test applies the full
set to an empty ephemeral Postgres database.
