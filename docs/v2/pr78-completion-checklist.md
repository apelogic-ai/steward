# PR #78 completion checklist

Scope: existing Task orchestration, legacy adoption, frozen compatibility, and staged
rollout on `feat/v2-common-core-sprint`. No new product features or gate changes.

Architecture: [Task-runtime orchestration](../task-runtime-orchestration.md), particularly
the completion transition matrix. Compatibility correction:
[legacy adopted runtimes](legacy-adopted-runtime-orchestration-correction-ticket.md).

Delivery priority (maintainer direction, 2026-09-08): deliver a P1-free candidate
first. Standalone P2 follow-up must not delay it. Retain already implemented fixes
that support P1 safety; do not expand approval-delivery or compatibility work beyond
what is necessary to validate the candidate. Existing mandatory gates remain required.

`Unresolved` includes implementation awaiting required evidence. A green focused test is
not a substitute for the real-stack exit criterion. P2 residual concerns must be
explicitly recorded as follow-up rather than represented as fixed.

| Finding / commitment | Status | Required closing evidence |
|---|---|---|
| Staged ownership and legacy-controller exclusion | Unresolved: candidate implemented | Mixed-version rollout and submission fences |
| Active authority revalidation and terminal-phase preservation | Unresolved: candidate implemented | Postgres authority-loss matrix, including before/after start |
| Claimed but never-authorized attempt cleanup | Focused Postgres regression green; candidate validation pending | Real Postgres `not_started`, start/cancel race, cleanup, restart |
| Unknown-outcome runtime quarantine and retirement | Postgres concurrency/restart/late-retirement regression green; pinned runtime evidence pending | Concurrent same-UID Tasks, restart/finalization persistence, late retirement and safe reuse |
| Retry transient attempt observations | Unresolved: candidate implemented | Transport fault followed by durable observation recovery |
| Orphaned claimed/running marker liveness | Unresolved: candidate implemented | Pinned OpenShell process/stream failures, not source-text assertions |
| Cancellation with absent marker/runtime/references | Unresolved: candidate implemented | Before/after deadline; no false retirement evidence |
| Cleanup of pending approval/outbox and active grants | Unresolved: candidate implemented | Claimed delivery, terminal approval and cleanup races |
| Approval delivery lease expiry and duplicate external effects (P2) | Existing focused race regression green; no further standalone expansion in P1 slice | Two delivery workers with slow/ambiguous external request |
| Shared runtime readiness lag versus identity drift | Unresolved | Wait through transient readiness; reject exact UID/spec mismatch |
| Historical adopted retry (P1) and finalized DELETE retry (P2) | Focused retry and historical-migration regressions green; candidate validation pending | Upgrade with historical rows; no new operation or history mutation |
| Concurrent M1 reservation retry status (P2) | Existing focused retry regression green; no further standalone expansion in P1 slice | Lookup, conflict, and noninserted reservation all return identical 200 retry semantics |
| Legacy execute-and-detach; M1 server-owned identity | Unresolved: candidate implemented | Existing pinned full Task E2E retained and green |
| Immutable profile identity, packaged bundle and provider detach | Unresolved: candidate implemented | Existing profile/provider coverage and full CI on candidate |

## Candidate evidence

- Fresh Postgres: all 28 `s4_store` tests passed, including pre-start fencing,
  concurrent runtime ownership, late retirement, and historical migration.
- Fresh Postgres controller fault-path suite: `task_orchestration` passed again
  after the approval-port changes (2026-09-09 UTC).
- Workspace tests, formatting, warning-as-error Clippy, and static quality checks
  passed in `cargo xtask ci`. The pinned G-1 lane then failed during Kind API-server
  startup, before the egress assertion. Run
  `steward-g1-20260909013755-23420` was removed by the harness; full CI is not green.
- E2E all-target warning-as-error Clippy passed with the real OpenShell
  cancellation/orphan-process regression added. That regression's runtime result
  remains pending; compilation is not runtime evidence.

Delivery checklist:

- [ ] Empty database and historical upgrade verification.
- [ ] Complete real Postgres transition/concurrency/restart regression matrix.
- [ ] `cargo xtask ci` and all affected required integration/E2E lanes, serially.
- [ ] Pinned OpenShell process-liveness, cancellation uncertainty, and quarantine evidence.
- [ ] Run-owned infrastructure and credentials cleaned up; exact tested revision recorded.
- [ ] Protocol review has no unresolved P1 finding; residual P2 follow-up is explicit.
- [ ] Reviewable commits, accurate PR description, announced push to PR #78 branch.
- [ ] Human merge remains outside agent authority.
