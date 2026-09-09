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

- Governed-connections run `governed-connections-20260909054712-89781`
  failed on `a2982d9` after the stack became ready: internal Tasks repeatedly
  reported an unavailable immutable Envelope and the connection request failed.
  Run-owned resources were removed. A fresh-Postgres regression reproduced the
  missing internal-catalog resolution in provisioning; driving the actual API
  broker additionally exposed manifest hashes built with a Task UID different
  from the identity persisted by the connection store. The correction resolves
  and verifies the exact internal authority for creation, activation and
  unbound cleanup, and hashes the persisted operation/Task identity. Both
  `task_orchestration` tests pass against fresh Postgres, including tampered-pin
  rejection and exact-UID cleanup before/after observation. Full governed E2E
  and full CI on this correction remain pending.
- Full `cargo xtask ci` passed on `66a6e60` (production/tests `1bb5933`):
  workspace tests, warning-as-error Clippy, static checks and pinned G-1/G-2/G-4/G-5.
  G-1 run `g1-20260909054055-86143` passed in 210.63 seconds after preloading
  the unchanged gateway image; its pin, chart, deadline and assertion were not
  changed. G-2 passed in 3.49 seconds, G-4 in 28.51 seconds and G-5 in 27.36
  seconds. Cleanup inventory and `cargo xtask dev doctor` found no run-owned
  containers or Steward credential directories left behind.
- Task run `task-20260909052225-76709` on `66a6e60` passed certificate setup
  after the identical gateway image was preloaded. Its lifecycle test then
  failed after 410.17 seconds while requiring a revoked Task-owned runtime to
  remain present in `Pending`. Read-only Postgres evidence showed the approval
  Task succeeded and finalized, and its exact runtime was absent; the remaining
  pending runtime was the intentionally preserved same-name replacement from
  the stale-UID control. The architecture instead requires authority-loss
  cleanup and exact-UID deletion. Replacement of that intermediate-state
  expectation with final-cleanup assertions awaits maintainer approval.
  The later legacy adoption cases did not execute. Exact cluster, image tags,
  probe containers and credential directory were confirmed removed.
- Full `cargo xtask e2e-s4` passed on `f7c51d3` (production/tests `1bb5933`)
  in run `s4-20260909051236-71957`: all 28 real-Postgres tests passed at normal
  concurrency in 17.43 seconds, followed by the live instance-bound grant E2E
  in 27.27 seconds. This includes historical migration, pre-start fencing,
  quarantine/retirement and approval/finalization regressions. The native server
  release build passed; exact cluster, image tag and credential directory were
  confirmed removed. The earlier S4 connection failure did not recur in this
  completed candidate run.
- Task run `task-20260909044835-57089` on `84374e6` (production/test tree
  `1bb5933`) again failed during OpenShell 0.0.90 certificate-job setup with
  `DeadlineExceeded`, before any Task assertion. Bounded observations captured
  the gateway image pulling successfully in 75 seconds and the certificate
  container reporting `Completed`; they do not establish why the Job missed
  its deadline. Kubernetes API reads intermittently timed out, with no node
  OOM kill recorded. Exact cluster, tagged images, probe containers and the
  credential directory were confirmed removed. Both native arm64 sandbox
  startup/version probes passed for Codex 0.139.0 and 0.140.0. The native Task
  controller/server image prebuild also passed; neither prebuild nor probes
  substitute for the still-pending full Task E2E.
  Read-only inspection of the matching chart digest confirmed that its
  certificate Job has a 120-second active deadline and uses `IfNotPresent`
  image pulls. The observed 75-second pull consumed much of that deadline;
  preloading the same pinned image is the next setup diagnostic, without
  changing the deadline, chart, or assertions.
- Candidate `1bb5933` passed the complete `cargo xtask ci` quality stage
  (workspace tests, warning-as-error Clippy and all static checks). Full CI then
  failed G-1 in run `g1-20260909034940-38329`: the pinned OpenShell 0.0.90
  sandbox's HTTP/2 stream broke before readiness, and the egress probe did not
  execute. Host Kubernetes API observations also timed out during this run;
  the node reported no OOM kills. The exact cluster and credential directory
  were removed. G-1 finding: runtime readiness/transport evidence is incomplete,
  not evidence that unlisted egress succeeded. Hold the existing pin and
  assertion unchanged; a successful candidate run remains required. Later
  pinned guarantees did not execute in this failed CI invocation.
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
- S4 Kubernetes run `s4-20260909030512-6162` built and deployed its server,
  Postgres and webhook, but its database test step lost connections: 15 passed,
  13 failed with unexpected EOF. The grant E2E did not run. The server image
  captured `b1977a3`; host tests used `5feebcc`. Run-owned resources were removed;
  one exited Kind node survived the harness and was removed by exact-cluster
  cleanup. The transport failure is not claimed resolved.
- A default-concurrency rerun against fresh direct Postgres exposed a separate
  approval-queue fixture collision (27 passed, cleanup matrix `ApprovalNotFound`).
  Three global queue consumers now use distinct test schemas, preserving each
  test's concurrent dispatchers and assertions. All 28 passed in three fresh
  default-concurrency runs (10.46s, 6.10s, 7.23s), and warning-as-error Clippy
  passed. Each Postgres instance was removed. No production queue selection,
  test thread count, or gate changed.
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
