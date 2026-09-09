# Migrations

SQL migrations are append-only. Slice S3 introduces Postgres operational state
for immutable envelope revisions, admission decisions, the approval queue, and
runtime-event history. Slice S4 adds immutable, runtime-UID-bound grants linked
to the Steward approval that authorized each exception. Migration 0011 adds
durable Task submission, execution, archive, and finalization state for the
single-shot Task API. Migration 0012 snapshots the admitting envelope revision
and adds append-only, provenance-marked Task lifecycle events for the
administrator Agent Runs read model; existing Task anchors are explicitly
backfilled rather than represented as complete history.

Migration 0012 introduces opaque canonical person identities. Existing Task
rows remain explicitly `legacy_reconnect_required`; the migration never derives
a person ID from email, issuer, or another mutable claim.

Migration 0015 appends an immutable, separately-recorded approved Envelope
snapshot to user-request lifecycle events. Older events intentionally have no
snapshot rather than inferring one from the original request.

Migration 0028 drains legacy in-flight Task rows and introduces the durable
Task-runtime orchestration boundary described in
[`docs/task-runtime-orchestration.md`](../docs/task-runtime-orchestration.md).
It atomically records immutable Task intent and an operation generation, exact
runtime UID observations, one observable execution attempt, recoverable
external-effect outbox state, append-only transition history, and absence
evidence required before finalization. New v2 writers are fenced from the old
nullable-UID lifecycle by database constraints and monotonic transition
triggers.

`cargo xtask migrate-check` rejects edits or renames of migrations already
present on the comparison base. The S3 and S4 store integration tests apply the
full set to empty ephemeral Postgres databases.

Migration 0029 introduced a one-active-attempt-per-UID predicate. Migration 0030
adds `not_started`, fencing never-authorized claims without fabricating a start.
Migration 0031 supersedes the state-based predicate with explicit runtime leases
and append-only execution retirement evidence. Unknown outcomes keep their lease
through finalization; only proven non-start or an exact adapter terminal observation
permits reuse. Late evidence does not rewrite terminal Task or attempt history.

The 0031 upgrade backfills known terminal observations and preserves every unknown
lease. It fails closed if older state already contains overlapping unknown/live
attempts on one UID; it does not silently select a winner. Keep writers staged and
resolve the original execution environments before migration, rather than inventing
retirement evidence or deleting immutable history.

Migration 0032 records approval delivery invocation separately from its scheduling
lease. Lease successors observe the original request instead of creating again;
cleanup cannot retire an invoked delivery before its external reference is known.
