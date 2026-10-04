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

Migration 0033 distinguishes transient internal connection output from immutable
execution evidence. After the matching connection result becomes terminal, its
successful Task's response archive can be cleared in the same transaction that
requests cleanup. The database records an immutable retirement timestamp and
rejects replacement or restoration. Ordinary Task output, finalized history,
runtime/attempt identity and result digests remain immutable. This is a narrowly
approved retention correction, not permission to rewrite an execution result.

Migration 0038 permits governed connection operations to select the Kubernetes
cluster-default runtime by storing an empty `runtime_class`. Whitespace-only
values remain invalid, and explicit nonblank RuntimeClass bindings are unchanged.

Migration 0039 removes Service Envelope authority from new and unfinished Task
orchestration. Version 3 persists either the exact approved User Envelope snapshot
or the existing code-owned internal authority pins. The upgrade resumes unfinished
v0.1.23 Tasks only when that authority is recoverable exactly and aborts rather
than guessing for unfinished legacy work. Terminal version 1/2 history remains
readable.

Migration 0040 adds the federated-subject observation, association, disable,
and append-only audit ledgers. It does not update or synthesize canonical users,
Tasks, runs, runtimes, Envelopes, or historical identity. The new
`steward-task-v3` writer remains disabled by default; older binaries ignore the
additive tables during a rolling upgrade. After v3 observations are written,
rollback requires disabling v3 first and preserving migration 0040 data.

Migration 0041 creates the immutable Envelope Template catalog, imports every
legacy member-role revision without changing its identity or content, and binds
existing requests to exact catalog revisions. New template writes use this
catalog; legacy role-keyed rows remain compatibility history.

Migration 0042 adds optional decision rationale, evidence, and expiry metadata,
plus an append-only external decision-reference ledger. Migration 0047 adds the
short-lived operational claim that serializes external decision filing without
weakening the immutable completed reference.

Migration 0043 creates append-only browser preference revisions for onboarding
dismissal and theme without turning identity-provider data into authority.

Migration 0044 adds instance-scoped cumulative spend grants and denials without
rewriting a template or approved request.

Migration 0045 creates and backfills typed Task stage events for admission,
runtime binding, execution start, and execution end, then records later stages
through database triggers. Migration 0046 adds bounded mutable live stdout and
stderr snapshots while leaving terminal execution logs immutable.

Migration 0048 adds append-only runtime-minute observations, exhaustion records,
instance-scoped grants, and denial decisions. Runtime usage is derived from
`task_lifecycle_events` running-to-terminal intervals, with a running Task clipped
at observation time and every interval clipped to the current UTC month. The same
migration adds a stable UUID public identifier to existing spend exhaustions so
the unified escalation API does not expose an internal sequence key.

Migration 0049 adds the governed GitHub workflow re-run operation. It admits the
immutable Steward connections authority v3, whose only additional grant is
`github/actions_run_trigger/write`, while preserving existing v1/v2 operation
rows. The operation kind remains database-allowlisted and invokes only MCP-GW's
`actions_run_trigger.rerun_workflow_run` method.

Migration 0050 adds the durable onboarding workflow acknowledgement to each
append-only browser preference revision. Existing revisions default to not
acknowledged; later preference writes carry the current value forward.

Migration 0051 permits a User Envelope request to omit both template identity
columns for an explicitly reviewed custom request, while a check constraint
continues to require both template ID and revision or neither. The existing
catalog foreign key still protects template-backed requests. The matching event
revision snapshot becomes nullable; no existing request or history is rewritten.
The catalog is documented as the sole authority for new template-backed writes,
while legacy role-keyed Envelope rows remain read-only for one compatibility
window.

Migration 0052 adds a nullable, bounded failure detail to governed connection
operations. Only the adapter-sanitized upstream HTTP status and optional reason
may be stored for `bridge-gateway-http` failures; raw provider responses,
request URLs, credentials, and arbitrary stderr remain outside this projection.

Migration 0053 adds the federated-subject association method and append-only
GitHub connection-verification evidence. Existing associated subjects are
classified from the audit event at their current revision: `v2_seeded` becomes
`v2-claim`, while an administrator association or replacement becomes `admin`.
Their canonical-user bindings and audit rows are unchanged.
New `connection_verified` audit events carry only provider `github` and the
bounded positive numeric account ID. Login, display name, and email are not
association evidence. The migration is additive and must remain in place if
automatic association is later disabled or v3 is rolled back.

Migration 0056 adds immutable, nullable Task source provenance populated only
from authenticated task-identity evidence. Existing direct-package Tasks are
deterministically backfilled from their immutable binding evidence; versioned
Tasks begin recording the same GitHub-ratified provenance on new reservations.
Other historical and Kubernetes-authenticated Tasks remain `NULL` and cannot be
used as GitHub Actions readiness evidence.

Migration 0057 adds the immutable authoring origin and browser Task evidence.
It classifies existing v3 Tasks as connections, GitHub Actions, or unknown from
their existing durable records. Migration 0054 preserves the finalized-Task
monotonicity trigger while allowing only those three new provenance fields
during the 0056/0057 backfills; their dedicated immutability triggers protect
the fields once they exist. Migration 0058 restores the ordinary strict
finalized-Task comparison.

The 0054/0058 compatibility pair is additive; no existing migration checksum is
changed. A database that failed before migration 0056 can retry normally after
the updated binary is deployed.

Migration 0059 preserves the two existing Task pin shapes and adds the distinct
browser direct-package shape: immutable browser evidence, no legacy Workflow
pin, and a complete approved User Envelope pin set. Published browser Workflows
continue to use the existing complete Workflow-and-Envelope pin shape.

Migration 0060 allows administrators to reserve a pending canonical member by
verified organization email and records the inviter in the existing append-only
identity audit. Migration 0061 adds optional OIDC display metadata and the last
successful browser sign-in timestamp without rewriting historical members. It
also records an administrator unlink as a new append-only federated-subject
audit action while returning the current subject to the observed pool.

Migration 0062 adds the administrator-managed member lifecycle. Disabled members
remain canonical identities and may be re-enabled; revoked invitations remain
immutable history but no longer reserve the organization email, so a later invite
creates a new pending member. Every transition remains append-only in the canonical
identity audit.

Migration 0063 extends the bounded governed-connection failure detail with MCP-GW's
optional machine-readable error code. Existing status-and-reason rows remain valid
and unchanged. New codes are limited to 100 lowercase ASCII letters, digits,
underscores, or hyphens; raw provider responses remain forbidden.
