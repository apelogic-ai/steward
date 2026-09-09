# PR #78 completion checklist

Scope: existing Task orchestration, legacy adoption, frozen compatibility, and staged
rollout on `feat/v2-common-core-sprint`. No new product features or gate changes.

Architecture: [Task-runtime orchestration](../task-runtime-orchestration.md), particularly
the completion transition matrix. Compatibility correction:
[legacy adopted runtimes](legacy-adopted-runtime-orchestration-correction-ticket.md).

Delivery cutoff (maintainer direction, 2026-09-08): fix demonstrated V2 demo
blockers; document other review findings in PR #78 rather than expanding this
slice. Retain already implemented safety fixes. Existing mandatory gates remain
required. Prepare the [GitOps assembly handoff](pr78-gitops-assembly-handoff.md)
with the exact pushed source revision and honest validation status.

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

- Legacy caller compatibility correction after `b1977a3`: reproduced both the
  initial adopted-null-UID response rejection and historical retry HTTP 200
  rejection using the exact pinned `steward-run` client source at
  `19230cc59a6b1246224912961e35c7044b0808d3` with mocked HTTP responses.
  Corrected response fixtures pass that parser. All 183 API-server unit tests
  pass, including legacy response projection across submission, retry, GET,
  upload, execute and DELETE while durable binding remains absent, and historical
  retry status. These are focused compatibility checks, not a full caller E2E;
  full CI evidence below predates this correction and must be rerun.
  The Task E2E target passes warning-as-error Clippy; its adoption check now
  observes the real Postgres binding separately from the legacy wire reference.
  The browser client was regenerated from OpenAPI, including its previously
  missing M1 HTTP 200 response variant.
- Fresh Postgres: all 28 `s4_store` tests passed, including pre-start fencing,
  concurrent runtime ownership, late retirement, and historical migration.
- Fresh Postgres controller fault-path suite: `task_orchestration` passed on
  `b482b2f`, including a temporary observation failure that leaves the accepted
  attempt and Task unchanged (2026-09-09 UTC).
- `cargo xtask ci` passed on the `b482b2f` production/test tree (only handoff
  documentation differs at `dde4889`): workspace tests, formatting,
  warning-as-error Clippy, static quality, and all four pinned guarantees.
  G-1 passed in run `g1-20260909022337-73843`, G-2 passed against pinned MCP-GW,
  and G-4/G-5 passed against pinned LiteLLM. Run-owned infrastructure and
  credential directories were removed. An earlier G-1 attempt failed during
  Kind API-server startup, before its assertion; this did not recur in the
  completed CI run. A sandbox advisory-cache permission error was resolved by
  permitting the unchanged gate to access Cargo's cache.
- E2E all-target warning-as-error Clippy passed with the real OpenShell
  cancellation/orphan-process regression added.
- `cargo xtask e2e-openshell-adapter` passed against OpenShell 0.0.98 in run
  `pr78-adapter-marker-20260909-0220`: authenticated copy, uncertain cancellation
  followed by success, and orphaned-wrapper detection without duplicate start.
  This used `b482b2f` plus a temporary error-only diagnostic, since removed.
  The harness removed the run-owned cluster and credential directory. Earlier
  attempts failed at initial transport connection and orphan-marker observation;
  neither reproduced in the passing run, so intermittent-failure diagnosis
  remains open rather than being represented as a production fix.
- `scripts/validate-release-artifacts.sh` passed chart/schema/parity and release
  helper checks. Its separate `--build-images` lane has not been verified here.
- `cargo xtask e2e-task` run `task-20260909023901-89647` failed during
  OpenShell's pre-install certificate-generation job timeout. No Task lifecycle
  assertion ran. Both sandbox versions had passed the native arm64 startup and
  executable-version preflight. The run-owned cluster, tagged images, and
  credential directory were removed; full Task runtime evidence remains pending.
- Web `bun run check` passed: lint, type-checking, and all 23 tests.

Delivery checklist:

- [ ] Empty database and historical upgrade verification.
- [ ] Complete real Postgres transition/concurrency/restart regression matrix.
- [ ] `cargo xtask ci` and all affected required integration/E2E lanes, serially.
- [ ] Pinned OpenShell process-liveness, cancellation uncertainty, and quarantine evidence.
- [ ] Run-owned infrastructure and credentials cleaned up; exact tested revision recorded.
- [ ] Demo-focused review has no unresolved demo blocker; other findings are explicit PR follow-up.
- [ ] Reviewable commits, accurate PR description, announced push to PR #78 branch.
- [ ] Human merge remains outside agent authority.
