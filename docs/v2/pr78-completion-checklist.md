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

The table below records evidence at the demo cutoff, not a promise to expand the
slice with another review cycle. The chronological run history retains earlier
failures; later passing runs supersede their then-current blocked status.

| Finding / commitment | Status | Required closing evidence |
|---|---|---|
| Staged ownership and legacy-controller exclusion | Guards implemented; real-Postgres legacy-writer rejection green | Assembly must still perform the documented staged rollout; no live rolling-upgrade exercise is claimed |
| Active authority revalidation and terminal-phase preservation | Real-Postgres matrix green | Authority loss before/after start; successful outcome remains immutable |
| Claimed but never-authorized attempt cleanup | Real-Postgres concurrency/restart regression green | `not_started`, start/cancel race, cleanup, restart |
| Unknown-outcome runtime quarantine and retirement | Postgres concurrency/restart/late-retirement and pinned adapter evidence green | Same-UID exclusion; uncertain cancellation followed by late terminal observation |
| Retry transient attempt observations | Controller fault-path regression green | Temporary observation error preserves durable rows; recovery observes the same attempt |
| Orphaned claimed/running marker liveness | Pinned OpenShell 0.0.98 adapter E2E green | Orphaned wrapper becomes uncertain; duplicate start prevented |
| Cancellation with absent marker/runtime/references | Database/controller fault-path regressions green | Deadlines do not manufacture execution-retirement evidence |
| Cleanup of pending approval/outbox and active grants | Real-Postgres matrix and full Task run's cleanup assertions passed | Full Task run subsequently failed at legacy adoption; not a whole-suite pass |
| Approval delivery lease expiry and duplicate external effects (P2) | Existing focused race regression green; no further standalone expansion in P1 slice | Two delivery workers with slow/ambiguous external request |
| Shared runtime readiness lag versus identity drift | Readiness and stale-UID controls passed before the later adoption failure | Full Task suite remains red |
| Historical adopted retry (P1) and finalized DELETE retry (P2) | Retry and real-Postgres historical-upgrade regressions green | No new operation or history mutation |
| Concurrent M1 reservation retry status (P2) | Existing focused retry regression green; no further standalone expansion in P1 slice | Lookup, conflict, and noninserted reservation all return identical 200 retry semantics |
| Legacy execute-and-detach | E2E acceptance retired by explicit maintainer direction, 2026-09-09 | Known HTTP 409 is not fixed; do not claim legacy adoption verified for this demo |
| M1 server-owned identity | Retained in V2 Task tests | Caller-selected runtime identity remains rejected |
| Immutable profile identity and provider detach | Profile/provider regressions and full CI green; both versioned Task executions passed | Full Task run later failed at adoption; packaged 1.2.0/0.140 compatibility remains an assembly prerequisite |

## Candidate evidence

- Maintainer explicitly directed removal of the legacy runtime adoption scenario:
  [approval in PR #78](https://github.com/apelogic-ai/steward/pull/78#issuecomment-5597899495).
  Only its standing-runtime fixture, adoption/execute/detach assertions and sole-use
  binding helper are retired. All V2 cases, including final failure cleanup, remain
  enabled. No production behavior, mandatory gate or conformance-register claim
  changes. The earlier 409 remains a known unverified compatibility path, not a
  fixed defect. A full V2-only Task run and final gate remain pending.
- Final Task run `task-20260909065936-32809` failed after 373.70 seconds at
  legacy adoption: HTTP 409, `adopted runtime does not match the resolved workflow
  and principal`. The copy, both versioned executions, stale-UID/readiness
  controls, approved revocation cleanup, three caller executions and scheduled
  execution had passed before this submission. Legacy execute-and-detach and the
  final failure-cleanup case did not complete. The API guard at
  `crates/steward-apiserver/src/tasks.rs` checks exact UID, namespace, full spec
  and absence of the pending-approval annotation; which comparison differed was
  not captured. Do not infer a confirmed root cause from the aggregate error.
  Source was `c14e2b2`, with formatter-only `49e87f3` applied before host test
  compilation. All production code is unchanged from green CI/governed checkpoint
  `6b460cc`. Under the cutoff, no automatic fix/review cycle was started. Full
  Task E2E remains red and no completed GitOps handoff or push is claimed. The
  run's credential directory, image aliases and probe containers were removed.
  Its stopped Kind node was confirmed run-owned and removed explicitly;
  `dev doctor` reports no Steward run artifacts.
- Maintainer explicitly approved the Task cleanup-expectation correction, isolated
  in `c14e2b2` (formatter-only follow-up `49e87f3`). The old expectation that a
  revoked Task-owned runtime remains Pending is false under durable orchestration.
  The test now requires exact UID absence, preserved success and byte-identical
  output, idempotent DELETE, and retired approval/outbox/grant authority. No
  production behavior or timeout changed. Full Task run
  `task-20260909065936-32809` passed these assertions and both native Codex probes,
  then failed at the separate legacy adoption step as recorded above.
- Full `cargo xtask ci` passed on `6b460cc`: formatting, workspace Clippy with
  warnings as errors, workspace tests, all static checks and pinned guarantees.
  G-1 passed in run `g1-20260909064820-28435` (175.68s), G-2 in 3.19s,
  G-4 in 26.26s and G-5 in 22.04s. The unchanged gateway image was preloaded;
  pins, deadlines and assertions were not changed. Exact run resources and
  credential directories were removed.
- Full governed-connections E2E passed on `6b460cc`, run
  `pr78-gov-202609090637`, in 225.96 seconds against pinned OpenShell 0.0.98.
  This covers status, OAuth start, disconnect, shared-runtime behavior and exact
  cleanup. A read-only check additionally confirmed that the successful OAuth
  start Task was finalized with its output absent, retirement timestamp present
  and result digest preserved. The exact cluster, image aliases, native probe
  and credential directory were removed; `dev doctor` reported no Steward run
  artifacts. The connection-output blocker below is resolved. The separately
  approved Task E2E expectation correction is recorded above.
- Maintainer approved the scoped internal connection-output retirement correction.
  New migration 0033 permits a consumed successful internal response to be
  cleared only with matching terminal connection and exact execution evidence,
  while requesting cleanup. A database-owned immutable retirement timestamp
  prevents restoration. No applied migration was edited; ordinary output and
  all outcome/identity/digest/finalized-history protections remain. All 29
  `s4_store` tests pass against fresh Postgres, including successful and rejected
  responses, populated 0032-to-0033 upgrade/restart, premature deletion,
  transaction rollback, ordinary-output protection and retirement/history
  tampering. Full governed E2E and candidate CI subsequently passed as recorded above.
- Governed-connections rerun `pr78-gov-202609090611` on `a0fb771` passed
  native arm64 preflight, pinned OpenShell 0.0.98 setup and internal runtime
  creation/activation. It then failed in 51.29 seconds: the connection finalizer
  clears transient Task output, but migration 0028 rejects that change as
  non-monotonic. A focused fresh-Postgres regression reproduces the failure
  after recording a successful execution through the real store transitions.
  Correction is pending approval for narrowly scoped internal-output retirement:
  execution outcome, identity and result digest must remain immutable, while
  consumed OAuth-bearing payloads must not be retained indefinitely. No
  constraint or test gate has been relaxed. The run's exact Kind node, image
  aliases, probe and credential directory are absent; `dev doctor` reports no
  Steward run artifacts. Full governed E2E remains red, not demo-ready.
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
